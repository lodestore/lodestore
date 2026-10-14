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
    Ok(())
}

pub fn check_write_version(config: &pb::Config) -> Result<()> {
    check_read_version(config)?;
    if config.min_writer_version > MAX_SUPPORTED_VERSION {
        return Err(unsupported("writer version", config.min_writer_version));
    }
    if !config.incompat_features.is_empty() {
        return Err(Error::InvalidInput(format!(
            "repository uses features this build cannot write: {}",
            config.incompat_features.join(", ")
        )));
    }
    Ok(())
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
        assert!(check_read_version(&c).is_ok());
        assert!(check_write_version(&c).is_err());
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
