use std::collections::BTreeMap;

use bytes::Bytes;
use lodestore_format::crypto::Entropy;
use lodestore_format::index::rebuild_index;
use lodestore_format::keys::KdfCost;
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::store::policy::{agent_iam_policy_json, agent_minio_policy_json};
use lodestore_format::store::probe::{
    AgentKeyState, AgentProbe, CHECK_ORDER, CheckId, CheckStatus, ProbeKeys, ProbeReport, probe,
    probe_keep, remove_probe_objects,
};
use lodestore_format::store::{
    BlobDelete, BlobError, BlobStore, OpendalStore, RepoLocation, StaticCredentials, Store,
};
use lodestore_testkit::fault_proxy::{FaultAction, FaultPlan, FaultProxy, FaultRule};
use lodestore_testkit::minio::{MinioServer, write_only_policy};
use tempfile::tempdir;

const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

const BUCKET: &str = "probe-bucket";

fn s3_store(minio: &MinioServer, key: &str, secret: &str) -> Store {
    RepoLocation::new("s3", [("endpoint", minio.endpoint()), ("bucket", BUCKET)])
        .open(Some(StaticCredentials {
            access_key_id: key.to_owned(),
            secret_access_key: secret.to_owned(),
        }))
        .unwrap()
}

async fn minio_with_bucket() -> (MinioServer, Store) {
    let minio = MinioServer::start().await;
    minio.create_bucket(BUCKET).await;
    let (key, secret) = minio.root_credentials();
    let root = s3_store(&minio, &key, &secret);
    (minio, root)
}

fn assert_order(report: &ProbeReport) {
    let ids: Vec<CheckId> = report.checks.iter().map(|c| c.id).collect();
    assert_eq!(ids, CHECK_ORDER);
}

fn statuses(report: &ProbeReport) -> Vec<CheckStatus> {
    report.checks.iter().map(|c| c.status).collect()
}

#[tokio::test]
async fn probe_s3_minio_agent_can_overwrite() {
    use CheckStatus::{Neutral, Pass, Warn};
    let (minio, root) = minio_with_bucket().await;
    let (key, secret) = minio
        .create_user_with_policy(BUCKET, &agent_minio_policy_json(BUCKET, ""))
        .await;
    let agent = s3_store(&minio, &key, &secret);

    let report = probe(&root, AgentProbe::Store(&agent)).await;
    assert_order(&report);
    assert_eq!(statuses(&report), [Pass, Pass, Pass, Warn, Neutral]);
    assert!(!report.existing_repository);
    assert!(!report.blocking());
    assert_eq!(report.agent_key_state(), AgentKeyState::CanOverwrite);
    let note = report.check(CheckId::AgentDelete).error.as_deref().unwrap();
    assert!(note.contains("overwrite"), "{note}");

    let mut entropy = Entropy::os();
    let options = InitOptions {
        kdf: LOW_KDF,
        ..Default::default()
    };
    PendingInit::generate(&mut entropy, options)
        .unwrap()
        .commit(&root, b"pw", &mut entropy)
        .await
        .unwrap();

    let report = probe(&root, AgentProbe::Store(&agent)).await;
    assert!(report.existing_repository);
    assert_eq!(statuses(&report), [Pass, Pass, Pass, Warn, Neutral]);

    let leftover = root.list("packs/probe-").await.unwrap();
    assert!(leftover.is_empty(), "{leftover:?}");
    root.put_if_absent("packs/probe-0123456789abcdef", Bytes::from_static(b"x"))
        .await
        .unwrap();
    let repo = Repository::open(&root, b"pw").await.unwrap();
    let rebuilt = rebuild_index(&root, repo.read_keys(), &|_, _| {})
        .await
        .unwrap();
    assert_eq!(rebuilt.packs_scanned, 0);
}

#[tokio::test]
async fn probe_s3_root_as_agent_can_delete() {
    let (minio, root) = minio_with_bucket().await;
    let (key, secret) = minio.root_credentials();
    let agent = s3_store(&minio, &key, &secret);

    let report = probe(&root, AgentProbe::Store(&agent)).await;
    assert_order(&report);
    assert_eq!(report.check(CheckId::AgentDelete).status, CheckStatus::Warn);
    assert_eq!(report.agent_key_state(), AgentKeyState::CanDelete);
    assert!(!report.blocking());
}

#[tokio::test]
async fn probe_s3_bad_credentials_blocks() {
    let (minio, _root) = minio_with_bucket().await;
    let (key, _) = minio.root_credentials();
    let secret = "not-the-secret-0123456789";
    let bad = s3_store(&minio, &key, secret);

    let report = probe(&bad, AgentProbe::NotEnforceable).await;
    assert_order(&report);
    let write = report.check(CheckId::Write);
    assert_eq!(write.status, CheckStatus::Fail);
    let error = write.error.as_deref().unwrap();
    assert!(!error.is_empty());
    assert!(!error.contains(secret), "{error}");
    assert!(report.blocking());
}

#[tokio::test]
async fn probe_s3_bad_server_key_skips_agent() {
    let (minio, root) = minio_with_bucket().await;
    let (key, _) = minio.root_credentials();
    let secret = "not-the-secret-0123456789";
    let bad = s3_store(&minio, &key, secret);
    let (agent_key, agent_secret) = minio
        .create_user_with_policy(BUCKET, &agent_minio_policy_json(BUCKET, ""))
        .await;
    let agent = s3_store(&minio, &agent_key, &agent_secret);

    let report = probe(&bad, AgentProbe::Store(&agent)).await;
    assert_order(&report);
    let write = report.check(CheckId::Write);
    assert_eq!(write.status, CheckStatus::Fail);
    assert_eq!(write.error.as_deref(), Some("access denied"));
    let detail = write.detail.as_deref().unwrap();
    assert!(!detail.is_empty());
    assert!(!detail.contains(secret), "{detail}");
    for id in [CheckId::AgentDelete, CheckId::ObjectLock] {
        let check = report.check(id);
        assert_eq!(check.status, CheckStatus::Neutral);
        assert_eq!(
            check.error.as_deref(),
            Some("skipped because the write check failed")
        );
    }
    assert_eq!(report.agent_key_state(), AgentKeyState::Unchecked);
    assert!(report.blocking());
    let leftover = root.list("packs/probe-").await.unwrap();
    assert!(leftover.is_empty(), "{leftover:?}");
}

#[tokio::test]
async fn probe_local_folder() {
    use CheckStatus::{Neutral, Pass};
    let dir = tempdir().unwrap();
    let store = RepoLocation::new("local", [("path", dir.path().to_str().unwrap())])
        .open(None)
        .unwrap();

    let report = probe(&store, AgentProbe::NotEnforceable).await;
    assert_order(&report);
    assert_eq!(statuses(&report), [Pass, Pass, Pass, Neutral, Neutral]);
    assert_eq!(
        report.check(CheckId::AgentDelete).error.as_deref(),
        Some("Not enforceable on a local folder")
    );
    assert!(!report.existing_repository);
    assert!(!report.blocking());
    assert_eq!(report.agent_key_state(), AgentKeyState::Unchecked);
}

#[tokio::test]
async fn probe_objects_can_be_removed_after_the_run() {
    let dir = tempdir().unwrap();
    let store = RepoLocation::new("local", [("path", dir.path().to_str().unwrap())])
        .open(None)
        .unwrap();
    let keys = ProbeKeys::default();

    let _ = probe_keep(&store, AgentProbe::NotEnforceable, &keys).await;
    assert_eq!(store.list("packs/probe-").await.unwrap().len(), 1);

    assert!(remove_probe_objects(&store, &keys).await.is_empty());
    assert!(store.list("packs/probe-").await.unwrap().is_empty());
}

#[tokio::test]
async fn probe_conditional_write_collides() {
    let (minio, root) = minio_with_bucket().await;
    let (key, secret) = minio.root_credentials();
    let agent = s3_store(&minio, &key, &secret);
    let _ = probe(&root, AgentProbe::Store(&agent)).await;

    let key = "packs/probe-00000000deadbeef";
    root.put_if_absent(key, Bytes::from_static(b"first"))
        .await
        .unwrap();
    let again = root.put_if_absent(key, Bytes::from_static(b"second")).await;
    assert!(matches!(again, Err(BlobError::AlreadyExists)));
    assert_eq!(&root.get(key).await.unwrap()[..], b"first");
}

#[test]
fn policy_matches_phase1_contract() {
    let minio: serde_json::Value = serde_json::from_str(&agent_minio_policy_json("b", "")).unwrap();
    assert_eq!(minio, write_only_policy("b"));

    let aws: serde_json::Value = serde_json::from_str(&agent_iam_policy_json("b", "")).unwrap();
    let mut statements = aws["Statement"].as_array().unwrap().clone();
    let deny = statements.pop().unwrap();
    assert_eq!(statements, minio["Statement"].as_array().unwrap().clone());
    assert_eq!(deny["Effect"], "Deny");
    assert_eq!(deny["Action"], serde_json::json!(["s3:PutObject"]));
    assert_eq!(deny["Resource"], statements[0]["Resource"]);
    assert_eq!(
        deny["Condition"],
        serde_json::json!({"Null": {"s3:if-none-match": "true"}})
    );

    let prefixed: serde_json::Value =
        serde_json::from_str(&agent_iam_policy_json("b", "p")).unwrap();
    let statements = prefixed["Statement"].as_array().unwrap();
    for st in [&statements[0], &statements[1], &statements[3]] {
        for r in st["Resource"].as_array().unwrap() {
            assert!(r.as_str().unwrap().starts_with("arn:aws:s3:::b/p/"), "{r}");
        }
    }
    assert_eq!(statements[2]["Resource"][0], "arn:aws:s3:::b");
    assert_eq!(
        statements[2]["Condition"]["StringLike"]["s3:prefix"],
        serde_json::json!(["p/index/*", "p/index/"])
    );
}

#[tokio::test]
async fn opendal_store_s3_scheme_roundtrip() {
    use CheckStatus::Pass;
    let (minio, _root) = minio_with_bucket().await;
    let (key, secret) = minio.root_credentials();
    let options: BTreeMap<String, String> = [
        ("bucket", BUCKET),
        ("endpoint", minio.endpoint()),
        ("region", "us-east-1"),
        ("access_key_id", key.as_str()),
        ("secret_access_key", secret.as_str()),
        ("root", "/"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    let store = OpendalStore::open("s3", &options).unwrap();
    assert!(store.conditional_write_supported());
    assert!(!format!("{store:?}").contains(&secret));

    store
        .put_if_absent("packs/obj", Bytes::from_static(b"hello world"))
        .await
        .unwrap();
    let again = store
        .put_if_absent("packs/obj", Bytes::from_static(b"other"))
        .await;
    assert!(matches!(again, Err(BlobError::AlreadyExists)));
    assert_eq!(&store.get("packs/obj").await.unwrap()[..], b"hello world");
    assert_eq!(
        &store.get_range("packs/obj", 6..11).await.unwrap()[..],
        b"world"
    );
    let listed = store.list("packs/").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!((listed[0].key.as_str(), listed[0].size), ("packs/obj", 11));
    store.delete("packs/obj").await.unwrap();
    assert!(matches!(
        store.get("packs/obj").await,
        Err(BlobError::NotFound)
    ));

    let store = Store::Opendal(store);
    let report = probe(&store, AgentProbe::NotEnforceable).await;
    assert_order(&report);
    assert_eq!(statuses(&report)[..3], [Pass, Pass, Pass]);
}

fn raw_operator(minio: &MinioServer, key: &str, secret: &str) -> opendal::Operator {
    opendal::install_default();
    let builder = opendal::services::S3::default()
        .endpoint(minio.endpoint())
        .bucket(BUCKET)
        .root("/")
        .region("us-east-1")
        .access_key_id(key)
        .secret_access_key(secret);
    opendal::Operator::new(builder).unwrap()
}

#[tokio::test]
async fn minio_lets_a_write_only_key_overwrite() {
    let (minio, root) = minio_with_bucket().await;
    let (key, secret) = minio
        .create_user_with_policy(BUCKET, &agent_minio_policy_json(BUCKET, ""))
        .await;
    root.put_if_absent("packs/victim", Bytes::from_static(b"original"))
        .await
        .unwrap();

    let agent = raw_operator(&minio, &key, &secret);
    agent
        .write("packs/victim", b"tampered".to_vec())
        .await
        .unwrap();
    assert_eq!(&root.get("packs/victim").await.unwrap()[..], b"tampered");
}

async fn proxied_agent(minio: &MinioServer, rules: Vec<FaultRule>) -> (FaultProxy, Store) {
    let (key, secret) = minio
        .create_user_with_policy(BUCKET, &agent_minio_policy_json(BUCKET, ""))
        .await;
    let plan = rules.into_iter().fold(FaultPlan::none(), FaultPlan::with);
    let proxy = FaultProxy::start(minio.endpoint(), plan).await;
    let endpoint = proxy.endpoint();
    let agent = RepoLocation::new("s3", [("endpoint", endpoint.as_str()), ("bucket", BUCKET)])
        .open(Some(StaticCredentials {
            access_key_id: key,
            secret_access_key: secret,
        }))
        .unwrap();
    (proxy, agent)
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
async fn probe_s3_enforcing_provider_agent_verified() {
    use CheckStatus::{Neutral, Pass};
    let (minio, root) = minio_with_bucket().await;
    let (_proxy, agent) =
        proxied_agent(&minio, vec![every("PUT", FaultAction::DenyUnconditional)]).await;

    let report = probe(&root, AgentProbe::Store(&agent)).await;
    assert_eq!(statuses(&report), [Pass, Pass, Pass, Pass, Neutral]);
    assert_eq!(report.agent_key_state(), AgentKeyState::Verified);
}

#[tokio::test]
async fn probe_s3_delete_refused_for_other_reasons_is_inconclusive() {
    let (minio, root) = minio_with_bucket().await;
    let (_proxy, agent) = proxied_agent(
        &minio,
        vec![
            every("PUT", FaultAction::DenyUnconditional),
            every("DELETE", FaultAction::NotImplemented),
        ],
    )
    .await;

    let report = probe(&root, AgentProbe::Store(&agent)).await;
    let delete = report.check(CheckId::AgentDelete);
    assert_eq!(delete.status, CheckStatus::Neutral);
    assert!(delete.error.as_deref().unwrap().starts_with("inconclusive"));
    assert_eq!(report.agent_key_state(), AgentKeyState::Unchecked);
    assert!(!report.blocking());
}

#[test]
fn opendal_unknown_scheme_rejected() {
    let err = OpendalStore::open("nosuchbackend", &BTreeMap::new()).unwrap_err();
    assert!(
        err.to_string().contains("is not available in this build"),
        "{err}"
    );
    let err = RepoLocation::new("nosuchbackend", [("endpoint", "host")])
        .open(None)
        .map(|_| ())
        .unwrap_err();
    assert!(err.to_string().contains("unknown backend"), "{err}");
}
