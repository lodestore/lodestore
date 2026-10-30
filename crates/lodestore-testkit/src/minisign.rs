//! Minisign signer for tests. Produces the same file layout as `minisign -S`.

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
use blake2::{Blake2b512, Digest};
use data_encoding::{BASE64, HEXUPPER};

pub struct TestSigner {
    pair: Ed25519KeyPair,
    key_id: [u8; 8],
}

impl TestSigner {
    pub fn generate() -> TestSigner {
        let pair = Ed25519KeyPair::generate().expect("generate ed25519 key");
        let mut key_id = [0u8; 8];
        getrandom::fill(&mut key_id).expect("random key id");
        TestSigner { pair, key_id }
    }

    pub fn public_key_text(&self) -> String {
        let mut raw = b"Ed".to_vec();
        raw.extend_from_slice(&self.key_id);
        raw.extend_from_slice(self.pair.public_key().as_ref());
        format!(
            "untrusted comment: minisign public key {}\n{}\n",
            HEXUPPER.encode(&self.key_id),
            BASE64.encode(&raw)
        )
    }

    pub fn sign(&self, data: &[u8], trusted_comment: &str) -> String {
        let digest = Blake2b512::digest(data);
        self.finish(b"ED", digest.as_slice(), trusted_comment)
    }

    pub fn sign_legacy(&self, data: &[u8], trusted_comment: &str) -> String {
        self.finish(b"Ed", data, trusted_comment)
    }

    fn finish(&self, alg: &[u8; 2], signed: &[u8], trusted_comment: &str) -> String {
        let sig = self.pair.sign(signed);
        let mut raw = alg.to_vec();
        raw.extend_from_slice(&self.key_id);
        raw.extend_from_slice(sig.as_ref());

        let mut global_msg = sig.as_ref().to_vec();
        global_msg.extend_from_slice(trusted_comment.as_bytes());
        let global = self.pair.sign(&global_msg);

        format!(
            "untrusted comment: signature from minisign secret key\n{}\ntrusted comment: {}\n{}\n",
            BASE64.encode(&raw),
            trusted_comment,
            BASE64.encode(global.as_ref())
        )
    }
}
