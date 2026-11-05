#[path = "../../lodestore-format/tests/adv_support/mod.rs"]
mod adv_support;
mod common;

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use adv_support::{attacker_keys, plant_blobs};
use common::{PASSWORD, cache_dir_of, init_repo, lode, pseudo_random, restored_path, stdout_of};
use lodestore_format::attest::{ensure_upgraded, read_attestations};
use lodestore_format::chunker::blob_id;
use lodestore_format::crypto::{BLOB_TYPE_DATA, BlobId, Entropy};
use lodestore_format::index::{hex32, parse_hex32};
use lodestore_format::pb;
use lodestore_format::provenance::{ProvenanceSet, Signer};
use lodestore_format::repo::Repository;
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, Writer};
use serde_json::Value;

const FILE_SIZE: usize = 100 << 10;
const YEAR_9999: i64 = 253_370_764_800;

struct Fx {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    src: PathBuf,
    target: PathBuf,
    content: Vec<u8>,
}

fn fixture() -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let (repo, src) = (root.join("repo"), root.join("src"));
    fs::create_dir(&src).unwrap();
    let content = pseudo_random(FILE_SIZE);
    fs::write(src.join("f"), &content).unwrap();
    init_repo(&repo);
    Fx {
        target: root.join("out"),
        _tmp: tmp,
        repo,
        src,
        content,
    }
}

async fn open(repo: &Path) -> (LocalFsStore, Repository) {
    let store = LocalFsStore::new(repo.to_path_buf());
    let repository = Repository::open(&store, PASSWORD.as_bytes()).await.unwrap();
    (store, repository)
}

async fn upgrade(repo: &Path) {
    let (store, repository) = open(repo).await;
    ensure_upgraded(&store, repository.read_keys(), &mut Entropy::os())
        .await
        .unwrap();
}

async fn provenance(repo: &Path) -> ProvenanceSet {
    let (store, repository) = open(repo).await;
    let set = read_attestations(&store, repository.read_keys())
        .await
        .unwrap();
    ProvenanceSet::from_attested(&set, repository.read_keys())
}

async fn plant_for(repo: &Path, content: &[u8]) -> BlobId {
    let (store, repository) = open(repo).await;
    let keys = attacker_keys(&repository);
    let id = blob_id(&keys.k_id, content);
    plant_blobs(&store, &keys, &[(id, BLOB_TYPE_DATA)]).await
}

fn summary(repo: &Path, src: &Path) -> Value {
    stdout_of(lode(repo).args(["backup", "--json"]).arg(src))
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["type"] == "backup_summary")
        .expect("a backup_summary line")
}

fn snapshot_id(summary: &Value) -> BlobId {
    parse_hex32(summary["snapshot_id"].as_str().unwrap()).unwrap()
}

fn assert_restores_f(fx: &Fx, spec: &str) {
    lode(&fx.repo)
        .args(["restore", spec, "--target"])
        .arg(&fx.target)
        .assert()
        .success();
    let restored = restored_path(&fx.target, &fx.src).join("f");
    assert_eq!(fs::read(restored).unwrap(), fx.content);
}

fn cache_file(repo: &Path, name: &str) -> PathBuf {
    let base = cache_dir_of(repo).join("lode");
    let id = fs::read_dir(&base)
        .unwrap()
        .next()
        .expect("a cache folder for the repository")
        .unwrap()
        .path();
    id.join(name)
}

fn cached_ids(repo: &Path, name: &str) -> BTreeSet<BlobId> {
    fs::read_to_string(cache_file(repo, name))
        .unwrap_or_default()
        .lines()
        .filter_map(parse_hex32)
        .collect()
}

fn attestation_count(repo: &Path) -> usize {
    fs::read_dir(repo.join("locks"))
        .map(|d| {
            d.filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().starts_with("attest-"))
                .count()
        })
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread")]
async fn backup_strict_skips_unattested_shard_and_signs() {
    let fx = fixture();
    upgrade(&fx.repo).await;
    plant_for(&fx.repo, &fx.content).await;

    let saved = summary(&fx.repo, &fx.src);
    let id = snapshot_id(&saved);
    assert_restores_f(&fx, "latest");
    let signed = provenance(&fx.repo).await;
    assert_eq!(signed.signer(&id), Some(Signer::Owner));
    let record = signed.get(&id).unwrap();
    assert!(record.host_id.is_empty() && record.job_id.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn backup_strict_fails_on_lying_attested_shard_then_recovers() {
    let fx = fixture();
    let liar = plant_for(&fx.repo, &fx.content).await;
    upgrade(&fx.repo).await;

    let failed = lode(&fx.repo).arg("backup").arg(&fx.src).assert().failure();
    let stderr = String::from_utf8_lossy(&failed.get_output().stderr).to_string();
    assert!(stderr.contains("does not check out"), "{stderr}");
    assert!(cached_ids(&fx.repo, "distrusted-shards").contains(&liar));
    assert!(provenance(&fx.repo).await.is_empty());

    let saved = summary(&fx.repo, &fx.src);
    let id = snapshot_id(&saved);
    assert_restores_f(&fx, &hex32(&id));
    let signed = provenance(&fx.repo).await;
    assert_eq!(signed.len(), 1);
    assert!(signed.contains(&id));
}

#[tokio::test(flavor = "multi_thread")]
async fn backup_strict_ignores_forged_parent() {
    let fx = fixture();
    upgrade(&fx.repo).await;
    let first = snapshot_id(&summary(&fx.repo, &fx.src));

    let (store, repository) = open(&fx.repo).await;
    let keys = repository.read_keys().write_keys(repository.config_hash());
    let mut writer = Writer::new(&store, keys, repository.config())
        .await
        .unwrap();
    let options = BackupOptions {
        time: Some(pb::Timestamp {
            seconds: YEAR_9999,
            nanos: 0,
        }),
        ..Default::default()
    };
    let forged = writer
        .backup(std::slice::from_ref(&fx.src), options, &|_| {})
        .await
        .unwrap()
        .snapshot_id;
    assert_ne!(forged, first);

    let second = summary(&fx.repo, &fx.src);
    assert_eq!(second["parent"], hex32(&first));
}

#[tokio::test(flavor = "multi_thread")]
async fn backup_strict_caches_verified_shards() {
    let fx = fixture();
    lode(&fx.repo).arg("backup").arg(&fx.src).assert().success();
    upgrade(&fx.repo).await;
    fs::write(fx.src.join("g"), pseudo_random(1000)).unwrap();
    summary(&fx.repo, &fx.src);

    let shards: BTreeSet<BlobId> = fs::read_dir(fx.repo.join("index"))
        .unwrap()
        .filter_map(|e| parse_hex32(e.unwrap().file_name().to_str().unwrap()))
        .collect();
    assert!(shards.len() >= 2);
    assert_eq!(cached_ids(&fx.repo, "verified-shards"), shards);
    let mode = fs::metadata(cache_file(&fx.repo, "verified-shards"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn backup_strict_works_without_a_cache_folder() {
    let fx = fixture();
    let (rt, ()) = (tokio::runtime::Runtime::new().unwrap(), ());
    rt.block_on(upgrade(&fx.repo));
    let out = lode(&fx.repo)
        .env_remove("XDG_CACHE_HOME")
        .env_remove("HOME")
        .arg("backup")
        .arg(&fx.src)
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert_eq!(stderr.matches("no cache folder").count(), 1, "{stderr}");
}

#[test]
fn backup_legacy_repo_writes_no_attestation() {
    let fx = fixture();
    lode(&fx.repo).arg("backup").arg(&fx.src).assert().success();
    assert_eq!(attestation_count(&fx.repo), 0);
}
