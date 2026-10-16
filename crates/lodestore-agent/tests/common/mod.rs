use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lodestore_agent::Paths;
use lodestore_agent::identity::{write_json_file, write_private_file};
use lodestore_format::crypto::Entropy;
use lodestore_format::keys::KdfCost;
use lodestore_format::reader::Reader;
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::snapshot::SnapshotEntry;
use lodestore_format::store::LocalFsStore;

const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

pub struct TestAgent {
    pub paths: Paths,
    pub agent_id: String,
    pub repo_root: PathBuf,
    pub password: String,
}

pub async fn local_agent(dir: &Path) -> TestAgent {
    let repo_root = dir.join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();
    let store = LocalFsStore::new(repo_root.clone());
    let mut entropy = Entropy::os();
    let pending = PendingInit::generate(
        &mut entropy,
        InitOptions {
            kdf: LOW_KDF,
            ..InitOptions::default()
        },
    )
    .expect("generating a repository");
    let config_hash = lodestore_format::config::config_hash(pending.config_bytes());
    let writer_keys = pending.read_keys().write_keys(config_hash).to_file_text();
    let password = "test-password".to_string();
    pending
        .commit(&store, password.as_bytes(), &mut entropy)
        .await
        .expect("committing the repository");

    let paths = Paths {
        config_dir: dir.join("etc"),
        state_dir: dir.join("var"),
    };
    write_private_file(&paths.writer_keys(), writer_keys.as_bytes()).unwrap();
    write_json_file(
        &paths.repository_json(),
        &serde_json::json!({
            "repository_id": "test",
            "backend": "local",
            "options": BTreeMap::from([("path", repo_root.display().to_string())]),
        }),
    )
    .unwrap();
    TestAgent {
        paths,
        agent_id: "agent-test".to_string(),
        repo_root,
        password,
    }
}

pub async fn snapshots(agent: &TestAgent) -> Vec<SnapshotEntry> {
    let store = LocalFsStore::new(agent.repo_root.clone());
    let repo = Repository::open(&store, agent.password.as_bytes())
        .await
        .unwrap();
    let reader = Reader::open(&store, repo.read_keys().duplicate(), repo.config()).unwrap();
    reader.list_snapshots().await.unwrap().entries
}

#[allow(dead_code)]
pub fn short_socket_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ls")
        .tempdir_in("/tmp")
        .unwrap()
}
