//! A host with writer credentials works against a repository that was upgraded
//! for server signatures. Only the public API and commands of the first
//! release are used, so the scenarios can run against that release unchanged.

mod adv_support;
mod common;
#[path = "../../lodestore-format/tests/adv_support/mod.rs"]
mod planted;

use std::path::{Path, PathBuf};
use std::process::Output;

use common::{PASSWORD, lode, restored_path};
use lodestore_format::chunker::blob_id;
use lodestore_format::crypto::BLOB_TYPE_DATA;
use lodestore_format::pb;
use lodestore_format::repo::Repository;
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, Writer};

const YEAR_9999: i64 = 253_370_764_800;

struct Fx {
    tmp: tempfile::TempDir,
    repo: PathBuf,
}

impl Fx {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let repo = adv_support::upgraded_repo(tmp.path());
        Self { tmp, repo }
    }

    fn src(&self, name: &str, files: &[(&str, &[u8])]) -> PathBuf {
        let dir = self.tmp.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        for (file, bytes) in files {
            std::fs::write(dir.join(file), bytes).unwrap();
        }
        dir
    }

    fn target(&self, name: &str) -> PathBuf {
        self.tmp.path().join(name)
    }

    async fn open(&self) -> (LocalFsStore, Repository) {
        let store = LocalFsStore::new(self.repo.clone());
        let repo = Repository::open(&store, PASSWORD.as_bytes()).await.unwrap();
        (store, repo)
    }

    async fn plant_blob_of(&self, content: &[u8]) {
        let (store, repo) = self.open().await;
        let keys = planted::attacker_keys(&repo);
        let id = blob_id(&keys.k_id, content);
        planted::plant_blobs(&store, &keys, &[(id, BLOB_TYPE_DATA)]).await;
    }

    async fn plant_forged(&self, src: &Path) {
        let (store, repo) = self.open().await;
        let mut writer = Writer::new(&store, planted::attacker_keys(&repo), repo.config())
            .await
            .unwrap();
        let options = BackupOptions {
            time: Some(pb::Timestamp {
                seconds: YEAR_9999,
                nanos: 0,
            }),
            ..Default::default()
        };
        writer
            .backup(&[src.to_path_buf()], options, &|_| {})
            .await
            .unwrap();
    }

    fn backup(&self, src: &Path) -> Output {
        lode(&self.repo)
            .args(["backup", "--json"])
            .arg(src)
            .output()
            .unwrap()
    }

    fn restore_latest(&self, target: &Path) -> Output {
        lode(&self.repo)
            .args(["restore", "latest", "--target"])
            .arg(target)
            .output()
            .unwrap()
    }

    /// Whether `latest` restores `file` of `src` with exactly `bytes`.
    fn latest_restores(&self, src: &Path, file: &str, bytes: &[u8], target: &Path) -> bool {
        self.restore_latest(target).status.success()
            && std::fs::read(restored_path(target, src).join(file)).is_ok_and(|got| got == bytes)
    }
}

fn describe(out: &Output) -> String {
    format!(
        "exit {:?}, stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn summary(out: &Output) -> serde_json::Value {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["type"] == "backup_summary")
        .unwrap_or_else(|| panic!("no backup_summary: {}", describe(out)))
}

fn delete_attestations(repo: &Path) {
    for entry in std::fs::read_dir(repo.join("locks")).unwrap() {
        let path = entry.unwrap().path();
        if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("attest-"))
        {
            std::fs::remove_file(path).unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn adv_lode_backup_skips_unattested_planted_shard() {
    let fx = Fx::new();
    let content = vec![b'G'; 3000];
    fx.plant_blob_of(&content).await;
    let src = fx.src("src", &[("g", &content)]);

    let out = fx.backup(&src);
    if out.status.success() {
        assert!(
            fx.latest_restores(&src, "g", &content, &fx.target("out")),
            "backup exited 0 but the file named by a planted shard cannot be restored"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn adv_lode_backup_fails_on_lying_attested_shard() {
    let fx = Fx::new();
    let content = adv_support::lying_content();
    let src = fx.src("src", &[("f", &content)]);

    let first = fx.backup(&src);
    if first.status.success() {
        assert!(
            fx.latest_restores(&src, "f", &content, &fx.target("out1")),
            "backup exited 0 and the file named by a lying shard cannot be restored ({})",
            describe(&first)
        );
    }
    let second = fx.backup(&src);
    assert!(second.status.success(), "{}", describe(&second));
    assert!(
        fx.latest_restores(&src, "f", &content, &fx.target("out2")),
        "second backup is not restorable ({})",
        describe(&second)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn adv_planted_snapshot_is_never_chosen_as_parent() {
    let fx = Fx::new();
    let src = fx.src("src", &[("f", b"some data")]);
    let first = fx.backup(&src);
    assert!(first.status.success(), "{}", describe(&first));
    let h1 = summary(&first)["snapshot_id"].as_str().unwrap().to_owned();
    fx.plant_forged(&src).await;

    let second = fx.backup(&src);
    assert!(second.status.success(), "{}", describe(&second));
    assert_eq!(
        summary(&second)["parent"].as_str(),
        Some(h1.as_str()),
        "the parent of the next backup is not the honest snapshot ({})",
        describe(&second)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn adv_lode_restore_latest_ignores_forged_future_snapshot() {
    let fx = Fx::new();
    let src = fx.src("src", &[("f", b"honest")]);
    let evil = fx.src("evil", &[("e", b"forged")]);
    let backed = fx.backup(&src);
    assert!(backed.status.success(), "{}", describe(&backed));
    fx.plant_forged(&evil).await;

    let target = fx.target("out");
    let restored = fx.restore_latest(&target);
    assert!(
        !restored_path(&target, &evil).join("e").exists(),
        "latest restored the forged snapshot ({})",
        describe(&restored)
    );
    assert!(
        fx.latest_restores(&src, "f", b"honest", &fx.target("out2")),
        "latest did not restore the honest snapshot ({})",
        describe(&restored)
    );
}

#[test]
fn adv_lode_writer_keys_backup_never_silently_unrestorable() {
    let fx = Fx::new();
    let content = adv_support::lying_content();
    let src = fx.src("src", &[("f", &content)]);
    let keys = fx.target("writer-keys");
    lode(&fx.repo)
        .args(["key", "export-writer", "--output"])
        .arg(&keys)
        .assert()
        .success();

    let out = lode(&fx.repo)
        .env_remove("LODE_PASSWORD")
        .arg("--writer-keys")
        .arg(&keys)
        .args(["backup", "--json"])
        .arg(&src)
        .output()
        .unwrap();
    if out.status.success() {
        assert!(
            fx.latest_restores(&src, "f", &content, &fx.target("out")),
            "backup with writer keys exited 0 and cannot be restored ({})",
            describe(&out)
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn adv_lode_restore_latest_after_attestations_deleted_is_not_forged() {
    let fx = Fx::new();
    let src = fx.src("src", &[("f", b"honest")]);
    let evil = fx.src("evil", &[("e", b"forged")]);
    let backed = fx.backup(&src);
    assert!(backed.status.success(), "{}", describe(&backed));
    delete_attestations(&fx.repo);
    fx.plant_forged(&evil).await;

    let target = fx.target("out");
    let restored = fx.restore_latest(&target);
    assert!(
        !restored_path(&target, &evil).join("e").exists(),
        "latest restored the forged snapshot after the attestations were deleted ({})",
        describe(&restored)
    );
    if restored.status.success() {
        assert!(
            std::fs::read(restored_path(&target, &src).join("f")).is_ok_and(|b| b == b"honest"),
            "latest restored something other than the honest snapshot"
        );
    }
}
