//! A compromised agent against S3-compatible storage on MinIO, including the
//! Backblaze B2 preset (no conditional writes, relies on bucket versions).
//! `adv_client_*` read the journal of a proxy in front of MinIO;
//! `adv_service_*` attack with the agent's own credentials. Real B2 is not
//! reachable from here (roadmap 999.6): MinIO stands in for its versioning.

use bytes::Bytes;
use lodestore_format::Error;
use lodestore_format::crypto::Entropy;
use lodestore_format::keys::KdfCost;
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::store::policy::agent_minio_policy_json;
use lodestore_format::store::probe::{AgentProbe, GuaranteeLevel, probe};
use lodestore_format::store::{
    BlobDelete, BlobError, BlobStore, RepoLocation, StaticCredentials, Store, put_named,
};
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_testkit::fault_proxy::{FaultAction, FaultPlan, FaultProxy, FaultRule};
use lodestore_testkit::minio::{MinioServer, write_only_policy};

const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};
const ORIGINAL: &[u8] = b"original pack bytes";
const FORGED: &[u8] = b"forged pack bytes";
const VICTIM: &str = "packs/victim";

fn store_via(
    endpoint: &str,
    bucket: &str,
    (key, secret): &(String, String),
    provider: Option<&str>,
) -> Store {
    let mut options = vec![("endpoint", endpoint), ("bucket", bucket)];
    if let Some(p) = provider {
        options.push(("provider", p));
    }
    RepoLocation::new("s3", options)
        .open(Some(StaticCredentials {
            access_key_id: key.clone(),
            secret_access_key: secret.clone(),
        }))
        .unwrap()
}

fn raw_operator(
    endpoint: &str,
    bucket: &str,
    (key, secret): &(String, String),
) -> opendal::Operator {
    opendal::install_default();
    let builder = opendal::services::S3::default()
        .endpoint(endpoint)
        .bucket(bucket)
        .root("/")
        .region("us-east-1")
        .access_key_id(key)
        .secret_access_key(secret);
    opendal::Operator::new(builder).unwrap()
}

async fn init_repo(store: &Store) {
    let mut entropy = Entropy::os();
    let options = InitOptions {
        kdf: LOW_KDF,
        ..Default::default()
    };
    PendingInit::generate(&mut entropy, options)
        .unwrap()
        .commit(store, b"pw", &mut entropy)
        .await
        .unwrap();
}

fn every(method: &'static str, action: FaultAction) -> FaultRule {
    FaultRule {
        method,
        key_prefix: String::new(),
        action,
        times: u32::MAX,
    }
}

#[tokio::test]
async fn adv_client_b2_preset_sends_no_if_none_match_and_no_delete() {
    const BUCKET: &str = "b2-client";
    let minio = MinioServer::start().await;
    minio.create_versioned_bucket(BUCKET).await;
    let root = minio.root_credentials();
    let agent_keys = minio
        .create_user_with_policy(BUCKET, &agent_minio_policy_json(BUCKET, ""))
        .await;
    let proxy = FaultProxy::start(minio.endpoint(), FaultPlan::none()).await;
    let server = store_via(minio.endpoint(), BUCKET, &root, Some("b2"));
    let agent = store_via(&proxy.endpoint(), BUCKET, &agent_keys, Some("b2"));
    init_repo(&server).await;

    let src = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("a.bin"), vec![7u8; 300_000]).unwrap();
    let repo = Repository::open(&server, b"pw").await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    let mut writer = Writer::new(&agent, keys, repo.config()).await.unwrap();
    let outcome = writer
        .backup(
            &[src.path().to_path_buf()],
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert!(!outcome.incomplete);

    let requests = proxy.requests();
    let puts = requests.iter().filter(|r| r.method == "PUT").count();
    assert!(puts >= 3, "{requests:?}");
    for r in &requests {
        assert!(
            matches!(r.method.as_str(), "GET" | "HEAD" | "PUT"),
            "the agent sent {r:?}"
        );
        assert!(
            r.headers
                .iter()
                .all(|(k, _)| !k.eq_ignore_ascii_case("if-none-match")),
            "{r:?}"
        );
        assert!(
            !r.query.as_deref().is_some_and(|q| q.contains("uploads")),
            "multipart upload: {r:?}"
        );
    }
}

/// What an attack with the agent's credentials achieved, judged with the
/// server's credentials afterwards.
#[derive(Debug)]
struct Outcome {
    overwrote: bool,
    deleted: bool,
    original_current: bool,
    original_readable: bool,
}

async fn attack(
    minio: &MinioServer,
    server: &Store,
    bucket: &str,
    attacker_endpoint: &str,
    keys: &(String, String),
    key: &str,
) -> Outcome {
    server
        .put_if_absent(key, Bytes::from_static(ORIGINAL))
        .await
        .unwrap();
    let op = raw_operator(attacker_endpoint, bucket, keys);
    let overwrote = op.write(key, FORGED.to_vec()).await.is_ok();
    let after_overwrite = server.get(key).await.ok();
    let deleted = op.delete(key).await.is_ok();

    let mut original_readable = after_overwrite.as_deref() == Some(ORIGINAL);
    for (id, marker) in minio.object_versions_all(bucket, key).await {
        if marker || original_readable {
            continue;
        }
        let path = format!("lode/{bucket}/{key}");
        let body = minio.mc(&["cat", "--version-id", &id, &path]).await;
        original_readable = body.is_ok_and(|b| b.as_bytes() == ORIGINAL);
    }
    let original_current = server.get(key).await.is_ok_and(|b| b[..] == *ORIGINAL);
    Outcome {
        overwrote,
        deleted,
        original_current,
        original_readable,
    }
}

/// The level a probe reports must not promise more than the attack left
/// standing.
fn assert_level_matches(what: &str, level: GuaranteeLevel, atomic: bool, o: &Outcome) {
    match level {
        GuaranteeLevel::CreateOnlyEnforced => assert!(
            !o.overwrote && !o.deleted && o.original_current,
            "{what}: create_only_enforced but {o:?}"
        ),
        GuaranteeLevel::OverwriteKeepsOldVersion => assert!(
            !o.deleted && o.original_readable,
            "{what}: overwrite_keeps_old_version but {o:?}"
        ),
        GuaranteeLevel::HostCanOverwriteOrDelete => assert!(
            o.overwrote || o.deleted || !atomic,
            "{what}: host_can_overwrite_or_delete but no attack worked: {o:?}"
        ),
        GuaranteeLevel::Unchecked => panic!("{what}: the probe measured nothing"),
    }
}

#[tokio::test]
async fn adv_service_b2_old_version_survives_agent_overwrite() {
    const BUCKET: &str = "b2-service";
    let minio = MinioServer::start().await;
    minio.create_versioned_bucket(BUCKET).await;
    let root = minio.root_credentials();
    let agent_keys = minio
        .create_user_with_policy(BUCKET, &agent_minio_policy_json(BUCKET, ""))
        .await;
    let server = store_via(minio.endpoint(), BUCKET, &root, Some("b2"));
    let agent = store_via(minio.endpoint(), BUCKET, &agent_keys, Some("b2"));

    server
        .put_if_absent(VICTIM, Bytes::from_static(ORIGINAL))
        .await
        .unwrap();
    raw_operator(minio.endpoint(), BUCKET, &agent_keys)
        .write(VICTIM, FORGED.to_vec())
        .await
        .unwrap();
    assert_eq!(&server.get(VICTIM).await.unwrap()[..], FORGED);
    assert!(matches!(
        agent.delete(VICTIM).await,
        Err(BlobError::Denied { .. })
    ));

    let versions = minio.object_versions(BUCKET, VICTIM).await;
    assert_eq!(versions.len(), 2, "{versions:?}");
    let path = format!("lode/{BUCKET}/{VICTIM}");
    let mut seen = Vec::new();
    for id in &versions {
        seen.push(minio.mc(&["cat", "--version-id", id, &path]).await.unwrap());
    }
    assert!(seen.iter().any(|b| b.as_bytes() == ORIGINAL), "{seen:?}");
}

#[derive(Clone, Copy)]
enum Keys {
    WriteOnly,
    AgentPolicy,
    Root,
}

#[tokio::test]
async fn adv_service_level_matches_attacks() {
    use GuaranteeLevel::{CreateOnlyEnforced, HostCanOverwriteOrDelete, OverwriteKeepsOldVersion};
    let minio = MinioServer::start().await;
    let root = minio.root_credentials();
    let endpoint = minio.endpoint().to_owned();

    // what, versioned bucket, key, proxy refuses unconditional writes, provider preset, level
    let configs = [
        (
            "plain bucket, write-only key",
            false,
            Keys::WriteOnly,
            false,
            None,
            HostCanOverwriteOrDelete,
        ),
        (
            "versioned bucket, write-only key",
            true,
            Keys::WriteOnly,
            false,
            None,
            OverwriteKeepsOldVersion,
        ),
        (
            "plain bucket, agent policy",
            false,
            Keys::AgentPolicy,
            false,
            None,
            HostCanOverwriteOrDelete,
        ),
        (
            "versioned bucket, b2 preset",
            true,
            Keys::AgentPolicy,
            false,
            Some("b2"),
            OverwriteKeepsOldVersion,
        ),
        (
            "provider enforcing if-none-match",
            false,
            Keys::AgentPolicy,
            true,
            None,
            CreateOnlyEnforced,
        ),
        (
            "root key",
            false,
            Keys::Root,
            false,
            None,
            HostCanOverwriteOrDelete,
        ),
    ];
    for (n, (what, versioned, keys, enforce, provider, expected)) in configs.into_iter().enumerate()
    {
        let bucket = format!("lvl-{n}");
        if versioned {
            minio.create_versioned_bucket(&bucket).await;
        } else {
            minio.create_bucket(&bucket).await;
        }
        let keys = match keys {
            Keys::WriteOnly => {
                minio
                    .create_user_with_policy(&bucket, &write_only_policy(&bucket).to_string())
                    .await
            }
            Keys::AgentPolicy => {
                minio
                    .create_user_with_policy(&bucket, &agent_minio_policy_json(&bucket, ""))
                    .await
            }
            Keys::Root => root.clone(),
        };
        let plan = if enforce {
            FaultPlan::none().with(every("PUT", FaultAction::DenyUnconditional))
        } else {
            FaultPlan::none()
        };
        let proxy = FaultProxy::start(&endpoint, plan).await;
        let server = store_via(&endpoint, &bucket, &root, provider);
        let agent = store_via(&proxy.endpoint(), &bucket, &keys, provider);

        let report = probe(&server, AgentProbe::Store(&agent)).await;
        assert_eq!(report.guarantee, expected, "{what}: {report:?}");
        let outcome = attack(&minio, &server, &bucket, &proxy.endpoint(), &keys, VICTIM).await;
        assert_level_matches(
            what,
            report.guarantee,
            agent.publishes_atomically(),
            &outcome,
        );
    }
}

#[tokio::test]
async fn adv_client_short_object_is_not_counted() {
    const BUCKET: &str = "short-object";
    let minio = MinioServer::start().await;
    minio.create_bucket(BUCKET).await;
    let root = minio.root_credentials();
    let server = store_via(minio.endpoint(), BUCKET, &root, None);
    server
        .put_if_absent("packs/short", Bytes::from(vec![1u8; 100]))
        .await
        .unwrap();

    // A write-only key may not stat the pack, and an S3 write is atomic: the
    // name is taken to hold the object that was meant.
    let write_only = minio
        .create_user_with_policy(BUCKET, &agent_minio_policy_json(BUCKET, ""))
        .await;
    let blind = store_via(minio.endpoint(), BUCKET, &write_only, None);
    put_named(&blind, "packs/short", vec![1u8; 4096])
        .await
        .unwrap();
    assert_eq!(server.object_size("packs/short").await.unwrap(), Some(100));

    // A key that can list and read packs sees the size differ.
    let mut policy: serde_json::Value =
        serde_json::from_str(&agent_minio_policy_json(BUCKET, "")).unwrap();
    let statements = policy["Statement"].as_array_mut().unwrap();
    statements.push(serde_json::json!({
        "Effect": "Allow",
        "Action": ["s3:GetObject"],
        "Resource": [format!("arn:aws:s3:::{BUCKET}/packs/*")],
    }));
    statements.push(serde_json::json!({
        "Effect": "Allow",
        "Action": ["s3:ListBucket"],
        "Resource": [format!("arn:aws:s3:::{BUCKET}")],
    }));
    let reader = minio
        .create_user_with_policy(BUCKET, &policy.to_string())
        .await;
    let sighted = store_via(minio.endpoint(), BUCKET, &reader, None);
    let err = put_named(&sighted, "packs/short", vec![1u8; 4096])
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NameBurned { .. }), "{err:?}");
}
