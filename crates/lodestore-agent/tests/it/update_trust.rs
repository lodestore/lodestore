use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use lodestore_agent::Paths;
use lodestore_agent::update::minisign::{
    Digest512, PublicKey, Signature, SignatureError, TrustedRelease, verify,
};
use lodestore_agent::update::trust::{PROJECT_KEY, trusted_keys};
use lodestore_testkit::minisign::TestSigner;

fn setup() -> (tempfile::TempDir, Paths, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: root.path().to_path_buf(),
        state_dir: root.path().join("state"),
    };
    let dir = paths.trusted_keys();
    std::fs::create_dir(&dir).unwrap();
    chmod(&dir, 0o755);
    (root, paths, dir)
}

fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn key_file(dir: &Path, name: &str, signer: &TestSigner, mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, signer.public_key_text()).unwrap();
    chmod(&path, mode);
    path
}

fn project_key() -> PublicKey {
    PublicKey::parse(PROJECT_KEY).unwrap()
}

#[test]
fn empty_config_dir_trusts_only_the_project_key() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: root.path().to_path_buf(),
        state_dir: root.path().join("state"),
    };
    assert_eq!(trusted_keys(&paths), vec![project_key()]);
}

#[test]
fn host_key_is_trusted_and_verifies_its_signatures() {
    let (_root, paths, dir) = setup();
    let signer = TestSigner::generate();
    key_file(&dir, "test.pub", &signer, 0o644);

    let keys = trusted_keys(&paths);
    assert_eq!(keys.len(), 2);
    assert_eq!(keys.first(), Some(&project_key()));

    let sig = Signature::parse(&signer.sign(b"binary", "lodestore-agent 1.2.3 x86_64")).unwrap();
    let mut digest = Digest512::new();
    digest.update(b"binary");
    assert_eq!(
        verify(&keys, &digest.finalize(), &sig),
        Ok("lodestore-agent 1.2.3 x86_64")
    );
}

#[test]
fn two_host_keys_both_verify_and_a_third_does_not() {
    let (_root, paths, dir) = setup();
    let old = TestSigner::generate();
    let new = TestSigner::generate();
    let third = TestSigner::generate();
    key_file(&dir, "old.pub", &old, 0o644);
    key_file(&dir, "new.pub", &new, 0o644);

    let keys = trusted_keys(&paths);
    assert_eq!(keys.len(), 3);

    let comment = "lodestore-agent 1.0.0 x86_64";
    let mut digest = Digest512::new();
    digest.update(b"binary");
    let digest = digest.finalize();
    for signer in [&old, &new] {
        let sig = Signature::parse(&signer.sign(b"binary", comment)).unwrap();
        assert_eq!(verify(&keys, &digest, &sig), Ok(comment));
    }
    let sig = Signature::parse(&third.sign(b"binary", comment)).unwrap();
    assert!(matches!(
        verify(&keys, &digest, &sig),
        Err(SignatureError::UntrustedKey)
    ));
}

#[test]
fn release_comments_of_0_2_parse_as_three_words() {
    let stable = TrustedRelease::parse("lodestore-agent 0.2.0 x86_64").unwrap();
    assert_eq!(
        (stable.version.as_str(), stable.arch.as_str()),
        ("0.2.0", "x86_64")
    );
    let rc = TrustedRelease::parse("lodestore-agent 0.3.0-rc.1 aarch64").unwrap();
    assert_eq!(
        (rc.version.as_str(), rc.arch.as_str()),
        ("0.3.0-rc.1", "aarch64")
    );
    assert!(matches!(
        TrustedRelease::parse("lodestore-agent 0.3.0-rc.1 aarch64 rc"),
        Err(SignatureError::Malformed(_))
    ));
}

#[test]
fn unsafe_or_invalid_key_files_are_skipped() {
    let (root, paths, dir) = setup();
    let good = TestSigner::generate();
    key_file(&dir, "good.pub", &good, 0o600);

    key_file(&dir, "writable.pub", &TestSigner::generate(), 0o666);
    key_file(&dir, "no-extension", &TestSigner::generate(), 0o644);

    let target = root.path().join("outside.pub");
    std::fs::write(&target, TestSigner::generate().public_key_text()).unwrap();
    chmod(&target, 0o644);
    symlink(&target, dir.join("link.pub")).unwrap();

    let big = dir.join("big.pub");
    let mut text = TestSigner::generate().public_key_text();
    text.push_str(&"#".repeat(5000));
    std::fs::write(&big, text).unwrap();
    chmod(&big, 0o644);

    let junk = dir.join("junk.pub");
    std::fs::write(&junk, "not a key\n").unwrap();
    chmod(&junk, 0o644);

    let keys = trusted_keys(&paths);
    assert_eq!(keys.len(), 2);
    assert_eq!(keys.first(), Some(&project_key()));
    assert_eq!(
        keys.get(1),
        Some(&PublicKey::parse(&good.public_key_text()).unwrap())
    );
}

#[test]
fn writable_trusted_keys_dir_is_ignored_entirely() {
    let (_root, paths, dir) = setup();
    key_file(&dir, "test.pub", &TestSigner::generate(), 0o644);
    chmod(&dir, 0o777);
    assert_eq!(trusted_keys(&paths), vec![project_key()]);
}

#[test]
fn symlinked_trusted_keys_dir_is_ignored() {
    let (root, paths, dir) = setup();
    std::fs::remove_dir(&dir).unwrap();
    let real = root.path().join("real-keys");
    std::fs::create_dir(&real).unwrap();
    chmod(&real, 0o755);
    key_file(&real, "test.pub", &TestSigner::generate(), 0o644);
    symlink(&real, &dir).unwrap();
    assert_eq!(trusted_keys(&paths), vec![project_key()]);
}
