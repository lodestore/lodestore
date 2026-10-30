#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

use aws_lc_rs::signature::{ED25519, UnparsedPublicKey};
use blake2::{Blake2b512, Digest};
use data_encoding::BASE64;

const MAX_TEXT: usize = 4096;
const MAX_TRUSTED_COMMENT: usize = 1024;
const UNTRUSTED_PREFIX: &str = "untrusted comment:";
const TRUSTED_PREFIX: &str = "trusted comment: ";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SignatureError {
    #[error("Signature check failed: key not trusted on this host.")]
    UntrustedKey,
    #[error("Signature check failed: the file does not match its signature.")]
    BadSignature,
    #[error("Signature check failed: the signed comment was changed.")]
    BadGlobalSignature,
    #[error("Signature check failed: only prehashed minisign signatures (ED) are accepted.")]
    UnsupportedAlgorithm,
    #[error("Signature check failed: {0}.")]
    Malformed(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKey {
    key_id: [u8; 8],
    key: [u8; 32],
}

impl PublicKey {
    pub fn parse(text: &str) -> Result<PublicKey, SignatureError> {
        if text.len() > MAX_TEXT {
            return Err(SignatureError::Malformed("public key is too long"));
        }
        let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
        let mut line = lines
            .next()
            .ok_or(SignatureError::Malformed("public key is empty"))?;
        if line.starts_with(UNTRUSTED_PREFIX) {
            line = lines
                .next()
                .ok_or(SignatureError::Malformed("public key has no key line"))?;
        }
        if lines.next().is_some() {
            return Err(SignatureError::Malformed("public key has extra lines"));
        }
        let raw = BASE64
            .decode(line.as_bytes())
            .map_err(|_| SignatureError::Malformed("public key is not valid base64"))?;
        let (alg, rest) = raw
            .split_first_chunk::<2>()
            .ok_or(SignatureError::Malformed("public key is too short"))?;
        let (key_id, rest) = rest
            .split_first_chunk::<8>()
            .ok_or(SignatureError::Malformed("public key is too short"))?;
        let key = <[u8; 32]>::try_from(rest)
            .map_err(|_| SignatureError::Malformed("public key has the wrong length"))?;
        if alg != b"Ed" {
            return Err(SignatureError::Malformed("unknown public key algorithm"));
        }
        Ok(PublicKey {
            key_id: *key_id,
            key,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    key_id: [u8; 8],
    sig: [u8; 64],
    trusted_comment: String,
    global: [u8; 64],
}

impl Signature {
    pub fn parse(text: &str) -> Result<Signature, SignatureError> {
        if text.len() > MAX_TEXT {
            return Err(SignatureError::Malformed("signature file is too long"));
        }
        let lines: Vec<&str> = text
            .lines()
            .map(|l| l.trim_end_matches('\r'))
            .filter(|l| !l.is_empty())
            .collect();
        let [untrusted, sig_line, trusted, global_line] = lines.as_slice() else {
            return Err(SignatureError::Malformed(
                "signature file must have four lines",
            ));
        };
        if !untrusted.starts_with(UNTRUSTED_PREFIX) {
            return Err(SignatureError::Malformed("missing untrusted comment"));
        }
        let trusted_comment = trusted
            .strip_prefix(TRUSTED_PREFIX)
            .ok_or(SignatureError::Malformed("missing trusted comment"))?;
        if trusted_comment.len() > MAX_TRUSTED_COMMENT {
            return Err(SignatureError::Malformed("trusted comment is too long"));
        }

        let raw = BASE64
            .decode(sig_line.trim().as_bytes())
            .map_err(|_| SignatureError::Malformed("signature is not valid base64"))?;
        let (alg, rest) = raw
            .split_first_chunk::<2>()
            .ok_or(SignatureError::Malformed("signature is too short"))?;
        let (key_id, rest) = rest
            .split_first_chunk::<8>()
            .ok_or(SignatureError::Malformed("signature is too short"))?;
        let sig = <[u8; 64]>::try_from(rest)
            .map_err(|_| SignatureError::Malformed("signature has the wrong length"))?;
        match alg {
            b"ED" => {}
            b"Ed" => return Err(SignatureError::UnsupportedAlgorithm),
            _ => return Err(SignatureError::Malformed("unknown signature algorithm")),
        }

        let global = BASE64
            .decode(global_line.trim().as_bytes())
            .map_err(|_| SignatureError::Malformed("global signature is not valid base64"))?;
        let global = <[u8; 64]>::try_from(global.as_slice())
            .map_err(|_| SignatureError::Malformed("global signature has the wrong length"))?;

        Ok(Signature {
            key_id: *key_id,
            sig,
            trusted_comment: trusted_comment.to_string(),
            global,
        })
    }
}

/// BLAKE2b-512 is used only as the minisign `ED` prehash; Lodestore IDs and MACs stay keyed BLAKE3.
#[derive(Default)]
pub struct Digest512(Blake2b512);

impl Digest512 {
    pub fn new() -> Digest512 {
        Digest512::default()
    }

    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    pub fn finalize(self) -> [u8; 64] {
        self.0.finalize().into()
    }
}

pub fn verify<'a>(
    keys: &[PublicKey],
    digest: &[u8; 64],
    sig: &'a Signature,
) -> Result<&'a str, SignatureError> {
    let mut candidates = keys.iter().filter(|k| k.key_id == sig.key_id).peekable();
    if candidates.peek().is_none() {
        return Err(SignatureError::UntrustedKey);
    }
    let key = candidates
        .find(|k| {
            UnparsedPublicKey::new(&ED25519, &k.key)
                .verify(digest, &sig.sig)
                .is_ok()
        })
        .ok_or(SignatureError::BadSignature)?;

    let mut signed = Vec::with_capacity(64 + sig.trusted_comment.len());
    signed.extend_from_slice(&sig.sig);
    signed.extend_from_slice(sig.trusted_comment.as_bytes());
    UnparsedPublicKey::new(&ED25519, &key.key)
        .verify(&signed, &sig.global)
        .map_err(|_| SignatureError::BadGlobalSignature)?;
    Ok(&sig.trusted_comment)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedRelease {
    pub version: String,
    pub arch: String,
}

impl TrustedRelease {
    pub fn parse(comment: &str) -> Result<TrustedRelease, SignatureError> {
        let words: Vec<&str> = comment.split_whitespace().collect();
        match words.as_slice() {
            ["lodestore-agent", version, arch] => Ok(TrustedRelease {
                version: (*version).to_string(),
                arch: (*arch).to_string(),
            }),
            _ => Err(SignatureError::Malformed(
                "signed comment must be `lodestore-agent <version> <arch>`",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use lodestore_testkit::minisign::TestSigner;

    use super::*;

    const PROJECT_KEY: &str = include_str!("../../keys/project.pub");

    const REAL_PUB: &str = include_str!("../../tests/fixtures/minisign/test.pub");
    const REAL_PAYLOAD: &[u8] = include_bytes!("../../tests/fixtures/minisign/payload.bin");
    const REAL_SIG: &str = include_str!("../../tests/fixtures/minisign/payload.bin.minisig");
    const LEGACY_SIG: &str =
        include_str!("../../tests/fixtures/minisign/payload.bin.legacy.minisig");

    fn digest(data: &[u8]) -> [u8; 64] {
        let mut d = Digest512::new();
        d.update(data);
        d.finalize()
    }

    fn signed(signer: &TestSigner, data: &[u8]) -> (Vec<PublicKey>, Signature, String) {
        let text = signer.sign(data, "lodestore-agent 1.2.3 x86_64");
        let keys = vec![PublicKey::parse(&signer.public_key_text()).unwrap()];
        (keys, Signature::parse(&text).unwrap(), text)
    }

    #[test]
    fn signer_roundtrip_returns_trusted_comment() {
        let signer = TestSigner::generate();
        let (keys, sig, _) = signed(&signer, b"binary");
        let comment = verify(&keys, &digest(b"binary"), &sig).unwrap();
        assert_eq!(
            TrustedRelease::parse(comment).unwrap(),
            TrustedRelease {
                version: "1.2.3".into(),
                arch: "x86_64".into()
            }
        );
    }

    #[test]
    fn changed_file_is_a_bad_signature() {
        let signer = TestSigner::generate();
        let (keys, sig, _) = signed(&signer, b"binary");
        assert_eq!(
            verify(&keys, &digest(b"binarY"), &sig),
            Err(SignatureError::BadSignature)
        );
    }

    #[test]
    fn changed_trusted_comment_is_a_bad_global_signature() {
        let signer = TestSigner::generate();
        let (keys, _, text) = signed(&signer, b"binary");
        let forged = text.replace("1.2.3 x86_64", "9.9.9 aarch64");
        let sig = Signature::parse(&forged).unwrap();
        assert_eq!(
            verify(&keys, &digest(b"binary"), &sig),
            Err(SignatureError::BadGlobalSignature)
        );
    }

    #[test]
    fn other_signers_key_is_untrusted() {
        let signer = TestSigner::generate();
        let other = TestSigner::generate();
        let (_, sig, _) = signed(&signer, b"binary");
        let keys = vec![PublicKey::parse(&other.public_key_text()).unwrap()];
        assert_eq!(
            verify(&keys, &digest(b"binary"), &sig),
            Err(SignatureError::UntrustedKey)
        );
    }

    #[test]
    fn legacy_signature_is_rejected() {
        let signer = TestSigner::generate();
        let text = signer.sign_legacy(b"binary", "lodestore-agent 1.2.3 x86_64");
        assert_eq!(
            Signature::parse(&text),
            Err(SignatureError::UnsupportedAlgorithm)
        );
    }

    #[test]
    fn real_minisign_vector_verifies() {
        let keys = vec![PublicKey::parse(REAL_PUB).unwrap()];
        let sig = Signature::parse(REAL_SIG).unwrap();
        let comment = verify(&keys, &digest(REAL_PAYLOAD), &sig).unwrap();
        assert_eq!(comment, "lodestore-agent 9.9.9 x86_64");
        assert_eq!(
            TrustedRelease::parse(comment).unwrap(),
            TrustedRelease {
                version: "9.9.9".into(),
                arch: "x86_64".into()
            }
        );
    }

    #[test]
    fn real_minisign_legacy_vector_is_rejected() {
        assert_eq!(
            Signature::parse(LEGACY_SIG),
            Err(SignatureError::UnsupportedAlgorithm)
        );
    }

    #[test]
    fn project_key_parses() {
        PublicKey::parse(PROJECT_KEY).unwrap();
    }

    #[test]
    fn trusted_release_needs_exactly_three_words() {
        assert!(TrustedRelease::parse("lodestore-agent 1.2.3 aarch64").is_ok());
        assert!(TrustedRelease::parse("lodestore-agent 1.2.3").is_err());
        assert!(TrustedRelease::parse("other 1.2.3 x86_64").is_err());
        assert!(TrustedRelease::parse("lodestore-agent 1.2.3 x86_64 extra").is_err());
    }

    #[test]
    fn parsers_never_panic_on_damaged_input() {
        let signer = TestSigner::generate();
        let sig_text = signer.sign(b"binary", "lodestore-agent 1.2.3 x86_64");
        let key_text = signer.public_key_text();
        for text in [sig_text.as_bytes(), key_text.as_bytes()] {
            for end in 0..text.len() {
                let cut = String::from_utf8_lossy(text.get(..end).unwrap_or_default());
                let _ = Signature::parse(&cut);
                let _ = PublicKey::parse(&cut);
            }
            for i in 0..text.len() {
                for flip in [0x00u8, 0xFF, 0x20, b'=', b'\n'] {
                    let mut damaged = text.to_vec();
                    if let Some(b) = damaged.get_mut(i) {
                        *b = flip;
                    }
                    let damaged = String::from_utf8_lossy(&damaged);
                    let _ = Signature::parse(&damaged);
                    let _ = PublicKey::parse(&damaged);
                }
            }
        }
        for _ in 0..10_000 {
            let mut len = [0u8; 2];
            getrandom::fill(&mut len).unwrap();
            let mut buf = vec![0u8; usize::from(u16::from_le_bytes(len)) % 512];
            getrandom::fill(&mut buf).unwrap();
            let random = String::from_utf8_lossy(&buf);
            let _ = Signature::parse(&random);
            let _ = PublicKey::parse(&random);
        }
    }

    #[test]
    fn oversized_text_is_rejected() {
        let big = "a".repeat(MAX_TEXT + 1);
        assert!(Signature::parse(&big).is_err());
        assert!(PublicKey::parse(&big).is_err());
    }
}
