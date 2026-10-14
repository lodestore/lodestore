//! Nothing a user would call content or metadata appears in the bucket: not
//! in any object's bytes and not in any object's name. On Linux the test also
//! covers a user xattr; run it with `scripts/test-linux.sh`.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use lodestore_format::crypto::Entropy;
use lodestore_format::keys::KdfCost;
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, Writer};

const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

const CONTENT: &str = "marker-content-4f1c9a7e-secret";
const FILE_NAME: &str = "marker-file-name-81d2b6c3.txt";
const DIR_NAME: &str = "marker-dir-name-0e5a77f4";
const LINK_TARGET: &str = "marker-symlink-target-b39c10d8";
const XATTR_VALUE: &str = "marker-xattr-value-6a2f94e1";
const HOSTNAME: &str = "marker-hostname-c47d03b2";
const TAG: &str = "marker-tag-95e81a6f";

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Every object below `dir` as `(key, bytes)`.
fn objects(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            objects(root, &path, out);
        } else {
            let key = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            out.push((key, fs::read(&path).unwrap()));
        }
    }
}

#[tokio::test]
async fn no_plaintext_in_bucket() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dir = src.join(DIR_NAME);
    fs::create_dir_all(&dir).unwrap();
    // Repeated so that zstd shrinks it: the marker must not survive compression either.
    fs::write(dir.join(FILE_NAME), CONTENT.repeat(10_000)).unwrap();
    symlink(LINK_TARGET, src.join("link")).unwrap();
    #[cfg(target_os = "linux")]
    {
        let file = dir.join(FILE_NAME);
        rustix::fs::lsetxattr(
            &file,
            "user.lode.marker",
            XATTR_VALUE.as_bytes(),
            rustix::fs::XattrFlags::empty(),
        )
        .expect("user xattrs must be supported here");
    }

    let root = tmp.path().join("repo");
    let store = LocalFsStore::new(root.clone());
    let mut entropy = Entropy::os();
    let options = InitOptions {
        kdf: LOW_KDF,
        ..Default::default()
    };
    PendingInit::generate(&mut entropy, options)
        .unwrap()
        .commit(&store, b"pw", &mut entropy)
        .await
        .unwrap();
    let repo = Repository::open(&store, b"pw").await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    let mut writer = Writer::new(&store, keys, repo.config()).await.unwrap();
    let outcome = writer
        .backup(
            std::slice::from_ref(&src),
            BackupOptions {
                hostname: Some(HOSTNAME.as_bytes().to_vec()),
                tags: vec![TAG.to_owned()],
                ..Default::default()
            },
            &|_| {},
        )
        .await
        .unwrap();
    assert!(!outcome.incomplete);

    let mut found = Vec::new();
    objects(&root, &root, &mut found);
    for prefix in ["config", "keys/", "packs/", "index/", "snapshots/"] {
        assert!(
            found.iter().any(|(k, _)| k.starts_with(prefix)),
            "the scan saw no object under {prefix}"
        );
    }
    // The scan reads real bytes: a plaintext pack magic is found.
    assert!(
        found
            .iter()
            .any(|(k, b)| k.starts_with("packs/") && contains(b, b"LODEPK"))
    );

    let markers = [
        CONTENT,
        FILE_NAME,
        DIR_NAME,
        LINK_TARGET,
        XATTR_VALUE,
        HOSTNAME,
        TAG,
    ];
    for (key, bytes) in &found {
        for marker in markers {
            assert!(
                !contains(key.as_bytes(), marker.as_bytes()),
                "{marker} in the name {key}"
            );
            assert!(
                !contains(bytes, marker.as_bytes()),
                "{marker} in the bytes of {key}"
            );
        }
    }
}
