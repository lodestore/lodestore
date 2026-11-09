use prost::Message;

use crate::crypto::RepoId;
use crate::error::{Error, Result};
use crate::pb;

pub const FORMAT_VERSION: u32 = 1;
pub const MAX_SUPPORTED_VERSION: u32 = 1;

pub const KEM_ID_XWING: u32 = 0x647a;
pub const KDF_ID_HKDF_SHA256: u32 = 0x0001;
pub const AEAD_ID_CHACHA20POLY1305: u32 = 0x0003;
pub const BLOB_AEAD_XCHACHA20POLY1305: u32 = 1;

pub const CHUNKER_FASTCDC_V2020: u32 = 1;
pub const CHUNK_MIN_SIZE: u32 = 262_144;
pub const CHUNK_AVG_SIZE: u32 = 1_048_576;
pub const CHUNK_MAX_SIZE: u32 = 4_194_304;
pub const CHUNK_NORMALIZATION: u32 = 1;

pub fn new_config(
    repo_id: RepoId,
    public_key: Vec<u8>,
    created: pb::Timestamp,
    program_version: &str,
) -> pb::Config {
    pb::Config {
        format_version: FORMAT_VERSION,
        min_reader_version: 1,
        min_writer_version: 1,
        incompat_features: Vec::new(),
        repo_id: repo_id.to_vec(),
        hpke: Some(pb::HpkeSuite {
            kem_id: KEM_ID_XWING,
            kdf_id: KDF_ID_HKDF_SHA256,
            aead_id: AEAD_ID_CHACHA20POLY1305,
            public_key,
        }),
        blob_aead_id: BLOB_AEAD_XCHACHA20POLY1305,
        chunker: Some(pb::ChunkerParams {
            algorithm: CHUNKER_FASTCDC_V2020,
            min_size: CHUNK_MIN_SIZE,
            avg_size: CHUNK_AVG_SIZE,
            max_size: CHUNK_MAX_SIZE,
            normalization: CHUNK_NORMALIZATION,
        }),
        key_epoch: 0,
        created: Some(created),
        program_version: program_version.to_owned(),
    }
}

pub fn encode(config: &pb::Config) -> Vec<u8> {
    config.encode_to_vec()
}

pub fn decode(bytes: &[u8]) -> Result<pb::Config> {
    pb::Config::decode(bytes).map_err(|e| Error::corrupt("config", format!("undecodable: {e}")))
}

pub fn config_hash(config_bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(config_bytes).as_bytes()
}

pub fn repo_id_of(config: &pb::Config) -> Result<RepoId> {
    config
        .repo_id
        .as_slice()
        .try_into()
        .map_err(|_| Error::corrupt("config", "repo_id must be 16 bytes"))
}

fn unsupported(what: &'static str, required: u32) -> Error {
    Error::UnsupportedVersion {
        what,
        required,
        supported: MAX_SUPPORTED_VERSION,
    }
}

pub fn check_read_version(config: &pb::Config) -> Result<()> {
    if config.format_version == 0 {
        return Err(Error::corrupt("config", "format_version is missing"));
    }
    if config.format_version > MAX_SUPPORTED_VERSION {
        return Err(unsupported("format version", config.format_version));
    }
    if config.min_reader_version > MAX_SUPPORTED_VERSION {
        return Err(unsupported("reader version", config.min_reader_version));
    }
    if !config.incompat_features.is_empty() {
        return Err(Error::InvalidInput(format!(
            "repository uses features this build cannot read or write: {}",
            config.incompat_features.join(", ")
        )));
    }
    Ok(())
}

pub fn check_write_version(config: &pb::Config) -> Result<()> {
    check_read_version(config)?;
    if config.min_writer_version > MAX_SUPPORTED_VERSION {
        return Err(unsupported("writer version", config.min_writer_version));
    }
    Ok(())
}

pub fn check_suite(config: &pb::Config) -> Result<()> {
    let hpke = config
        .hpke
        .as_ref()
        .ok_or_else(|| Error::corrupt("config", "hpke suite is missing"))?;
    if hpke.kem_id != KEM_ID_XWING
        || hpke.kdf_id != KDF_ID_HKDF_SHA256
        || hpke.aead_id != AEAD_ID_CHACHA20POLY1305
        || config.blob_aead_id != BLOB_AEAD_XCHACHA20POLY1305
    {
        return Err(Error::corrupt(
            "config",
            format!(
                "unsupported cipher suite (kem {:#06x}, kdf {:#06x}, aead {:#06x}, blob aead {})",
                hpke.kem_id, hpke.kdf_id, hpke.aead_id, config.blob_aead_id
            ),
        ));
    }
    Ok(())
}

/// The check a server runs before it uses a repository: the bytes in the
/// bucket must be the configuration it recorded when the repository was added.
pub fn check_recorded(
    config_bytes: &[u8],
    expected_hash: &[u8; 32],
    expected_repo_id: &RepoId,
) -> Result<pb::Config> {
    if config_hash(config_bytes) != *expected_hash {
        return Err(Error::corrupt(
            "config",
            "differs from the configuration recorded when the repository was added",
        ));
    }
    let config = decode(config_bytes)?;
    check_read_version(&config)?;
    check_suite(&config)?;
    if repo_id_of(&config)? != *expected_repo_id {
        return Err(Error::corrupt("config", "belongs to another repository"));
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> pb::Config {
        new_config(
            [1; 16],
            vec![0; 4],
            pb::Timestamp {
                seconds: 0,
                nanos: 0,
            },
            "test",
        )
    }

    #[test]
    fn config_version_gates() {
        let ok = base();
        assert!(check_read_version(&ok).is_ok());
        assert!(check_write_version(&ok).is_ok());

        let mut c = base();
        c.min_reader_version = 2;
        let err = check_read_version(&c).unwrap_err();
        assert!(matches!(
            err,
            Error::UnsupportedVersion {
                required: 2,
                supported: 1,
                ..
            }
        ));
        assert!(err.to_string().contains("version 2"));
        assert!(check_write_version(&c).is_err());

        let mut c = base();
        c.format_version = 2;
        assert!(matches!(
            check_read_version(&c).unwrap_err(),
            Error::UnsupportedVersion { required: 2, .. }
        ));

        let mut c = base();
        c.min_writer_version = 2;
        assert!(check_read_version(&c).is_ok());
        assert!(matches!(
            check_write_version(&c).unwrap_err(),
            Error::UnsupportedVersion { required: 2, .. }
        ));

        let mut c = base();
        c.incompat_features = vec!["future".into()];
        for err in [
            check_read_version(&c).unwrap_err(),
            check_write_version(&c).unwrap_err(),
        ] {
            assert!(matches!(err, Error::InvalidInput(_)));
            assert!(err.to_string().contains("cannot read or write"));
            assert!(err.to_string().contains("future"));
        }
    }

    fn recorded() -> (Vec<u8>, [u8; 32]) {
        let bytes = encode(&base());
        let hash = config_hash(&bytes);
        (bytes, hash)
    }

    #[test]
    fn check_recorded_accepts_the_recorded_config() {
        let (bytes, hash) = recorded();
        let config = check_recorded(&bytes, &hash, &[1; 16]).unwrap();
        assert_eq!(config, base());
    }

    #[test]
    fn check_recorded_refuses_another_hash() {
        let (bytes, mut hash) = recorded();
        hash[0] ^= 1;
        let err = check_recorded(&bytes, &hash, &[1; 16]).unwrap_err();
        assert!(matches!(err, Error::Corrupt { .. }));
        assert!(err.to_string().contains("recorded"));
    }

    #[test]
    fn check_recorded_refuses_another_repo_id() {
        let (bytes, hash) = recorded();
        let err = check_recorded(&bytes, &hash, &[2; 16]).unwrap_err();
        assert!(matches!(err, Error::Corrupt { .. }));
        assert!(err.to_string().contains("another repository"));
    }

    #[test]
    fn check_recorded_refuses_an_unknown_suite() {
        let mut c = base();
        c.blob_aead_id = 99;
        let bytes = encode(&c);
        let err = check_recorded(&bytes, &config_hash(&bytes), &[1; 16]).unwrap_err();
        assert!(err.to_string().contains("unsupported cipher suite"));

        let mut c = base();
        c.incompat_features = vec!["future".into()];
        let bytes = encode(&c);
        let err = check_recorded(&bytes, &config_hash(&bytes), &[1; 16]).unwrap_err();
        assert!(err.to_string().contains("cannot read or write"));
    }

    #[test]
    fn config_hpke_suite_ids() {
        let decoded = decode(&encode(&base())).unwrap();
        let hpke = decoded.hpke.unwrap();
        assert_eq!(hpke.kem_id, 0x647a);
        assert_eq!(hpke.kdf_id, 0x0001);
        assert_eq!(hpke.aead_id, 0x0003);
        assert_eq!(decoded.blob_aead_id, 1);
        let chunker = decoded.chunker.unwrap();
        assert_eq!(
            (chunker.min_size, chunker.avg_size, chunker.max_size),
            (262_144, 1_048_576, 4_194_304)
        );
    }
}
