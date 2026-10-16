mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use common::{Api, TestServer, backends};
use lodestore_format::crypto::Entropy;
use lodestore_format::index::hex32;
use lodestore_format::keys::{KdfCost, RecoveryKey};
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::store::policy::agent_minio_policy_json;
use lodestore_format::store::{RepoLocation, StaticCredentials, Store};
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_testkit::minio::MinioServer;
use serde_json::{Value, json};

const BUCKET: &str = "repo";
const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

fn creds(pair: &(String, String)) -> Value {
    json!({ "access_key_id": pair.0, "secret_access_key": pair.1 })
}

fn make_tree(root: &Path) {
    std::fs::create_dir_all(root.join("sub/deeper")).unwrap();
    std::fs::create_dir(root.join("empty")).unwrap();
    let mut big = vec![0u8; 700_000];
    getrandom::fill(&mut big).unwrap();
    std::fs::write(root.join("big.bin"), &big).unwrap();
    std::fs::write(root.join("top.txt"), "hello from the lost server").unwrap();
    std::fs::write(root.join("sub/deeper/script.sh"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(
        root.join("sub/deeper/script.sh"),
        std::fs::Permissions::from_mode(0o751),
    )
    .unwrap();
    std::os::unix::fs::symlink("top.txt", root.join("link")).unwrap();
}

fn assert_trees_equal(a: &Path, b: &Path) {
    let (ma, mb) = (
        std::fs::symlink_metadata(a).unwrap(),
        std::fs::symlink_metadata(b).unwrap(),
    );
    assert_eq!(ma.file_type(), mb.file_type(), "{}", a.display());
    if ma.is_symlink() {
        assert_eq!(
            std::fs::read_link(a).unwrap(),
            std::fs::read_link(b).unwrap()
        );
        return;
    }
    assert_eq!(
        ma.permissions().mode() & 0o7777,
        mb.permissions().mode() & 0o7777,
        "mode of {}",
        a.display()
    );
    if ma.is_dir() {
        let names = |p: &Path| {
            let mut v: Vec<_> = std::fs::read_dir(p)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            v.sort();
            v
        };
        let listed = names(a);
        assert_eq!(listed, names(b), "entries of {}", a.display());
        for name in listed {
            assert_trees_equal(&a.join(&name), &b.join(&name));
        }
    } else {
        assert!(
            std::fs::read(a).unwrap() == std::fs::read(b).unwrap(),
            "{}",
            a.display()
        );
    }
}

async fn connect(api: &Api, draft: &Value, recovery_key: &str) -> String {
    let response = api
        .post(
            "/api/v1/repositories/connect",
            &json!({ "draft": draft, "password": null, "recovery_key": recovery_key }),
        )
        .await;
    assert_eq!(response.status(), 201);
    let body: Value = response.json().await.unwrap();
    body["id"].as_str().unwrap().to_string()
}

async fn wait_for_rebuild(api: &Api, repository_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let all: Vec<Value> = api.get("/api/v1/catalog").await.json().await.unwrap();
        let status = all
            .into_iter()
            .find(|s| s["repository_id"] == repository_id)
            .expect("the repository has a catalog status");
        match status["phase"].as_str().unwrap() {
            "done" => return status,
            "failed" => panic!("the rebuild failed: {status}"),
            _ => {}
        }
        assert!(Instant::now() < deadline, "the rebuild did not finish");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn open_with_key(store: &Store, recovery_key: &RecoveryKey) -> Repository {
    Repository::open_recovery(store, recovery_key)
        .await
        .expect("the recovery key opens the repository")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disaster_db_deleted_restore_from_bucket_only() {
    for backend in backends() {
        let minio = MinioServer::start().await;
        minio.create_bucket(BUCKET).await;
        let root = minio.root_credentials();
        let agent = minio
            .create_user_with_policy(BUCKET, &agent_minio_policy_json(BUCKET, ""))
            .await;
        let location = RepoLocation::new(
            "s3",
            [
                ("bucket", BUCKET),
                ("endpoint", minio.endpoint()),
                ("path_style", "true"),
            ],
        );
        let store = location
            .open(Some(StaticCredentials {
                access_key_id: root.0.clone(),
                secret_access_key: root.1.clone(),
            }))
            .unwrap();
        let mut entropy = Entropy::os();
        let pending = PendingInit::generate(
            &mut entropy,
            InitOptions {
                kdf: LOW_KDF,
                ..InitOptions::default()
            },
        )
        .unwrap();
        let recovery_text = pending.recovery_key().encode().to_string();
        pending
            .commit(&store, b"the-original-password", &mut entropy)
            .await
            .unwrap();
        let recovery_key = RecoveryKey::parse(&recovery_text).unwrap();

        let src = tempfile::tempdir().unwrap();
        make_tree(src.path());
        let repo = open_with_key(&store, &recovery_key).await;
        let mut writer = Writer::new(
            &store,
            repo.read_keys().write_keys(repo.config_hash()),
            repo.config(),
        )
        .await
        .unwrap();
        let outcome = writer
            .backup(
                &[src.path().to_path_buf()],
                BackupOptions {
                    hostname: Some(b"host-a".to_vec()),
                    ..BackupOptions::default()
                },
                &|_| {},
            )
            .await
            .unwrap();
        assert!(!outcome.incomplete);
        let snapshot_id = hex32(&outcome.snapshot_id);

        let draft = json!({
            "name": "cloud",
            "location": {
                "backend": "s3",
                "options": { "bucket": BUCKET, "endpoint": minio.endpoint(), "path_style": "true" },
            },
            "server_credentials": creds(&root),
            "agent_credentials": creds(&agent),
        });

        let first = TestServer::start(&backend).await;
        let api = first.api().await;
        let repository_id = connect(&api, &draft, &recovery_text).await;
        let status = wait_for_rebuild(&api, &repository_id).await;
        assert_eq!(status["snapshots"], 1, "[{}]", backend.name());
        first.stop().await;

        let out = tempfile::tempdir().unwrap();
        let repo = open_with_key(&store, &recovery_key).await;
        let mut reader = Reader::open(&store, repo.read_keys().duplicate(), repo.config()).unwrap();
        reader
            .restore(
                &outcome.snapshot_id,
                out.path(),
                &RestoreOptions::default(),
                &|_| {},
            )
            .await
            .unwrap();
        let canonical = src.path().canonicalize().unwrap();
        assert_trees_equal(
            src.path(),
            &out.path().join(canonical.strip_prefix("/").unwrap()),
        );

        let second = TestServer::start(&backend).await;
        let api = second.api().await;
        assert!(
            api.get("/api/v1/restore/hosts")
                .await
                .json::<Vec<Value>>()
                .await
                .unwrap()
                .is_empty()
        );
        let repository_id = connect(&api, &draft, &recovery_text).await;
        let status = wait_for_rebuild(&api, &repository_id).await;
        assert_eq!(status["read"], 1, "[{}]", backend.name());
        assert_eq!(status["total"], 1);
        assert_eq!(status["added"], 1);

        let hosts: Vec<Value> = api.get("/api/v1/restore/hosts").await.json().await.unwrap();
        let host = hosts
            .iter()
            .find(|h| h["key"] == "agentless:host-a")
            .expect("the host appears from its snapshots");
        assert_eq!(host["snapshot_count"], 1);
        assert_eq!(host["agent"], false);

        let page: Value = api
            .get("/api/v1/restore/snapshots?host=agentless:host-a")
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(page["total"], 1);
        assert_eq!(page["items"][0]["id"], snapshot_id);
        second.stop().await;
    }
}
