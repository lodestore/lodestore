use bytes::Bytes;
use lodestore_format::store::policy::agent_minio_policy_json;
use lodestore_format::store::{
    BlobDelete, BlobError, BlobPurge, BlobStore, PurgeOutcome, RetainedVersion, S3Store,
    StaticCredentials,
};
use lodestore_testkit::minio::MinioServer;
use opendal::{Operator, services};

fn root_creds(minio: &MinioServer) -> StaticCredentials {
    let (access_key_id, secret_access_key) = minio.root_credentials();
    StaticCredentials {
        access_key_id,
        secret_access_key,
    }
}

fn open_store(minio: &MinioServer, bucket: &str) -> S3Store {
    S3Store::open(
        &format!("s3:{}/{bucket}", minio.endpoint()),
        Some(root_creds(minio)),
    )
    .unwrap()
}

fn operator(minio: &MinioServer, bucket: &str) -> Operator {
    let (key, secret) = minio.root_credentials();
    operator_as(minio, bucket, &key, &secret)
}

fn operator_as(minio: &MinioServer, bucket: &str, key: &str, secret: &str) -> Operator {
    opendal::install_default();
    Operator::new(
        services::S3::default()
            .endpoint(minio.endpoint())
            .bucket(bucket)
            .root("/")
            .region("us-east-1")
            .access_key_id(key)
            .secret_access_key(secret)
            .disable_ec2_metadata(),
    )
    .unwrap()
}

#[tokio::test]
async fn gc_purge_s3_opendal_versions() {
    let minio = MinioServer::start().await;
    minio.create_versioned_bucket("spike").await;
    let store = open_store(&minio, "spike");
    let key = "packs/aa/spike";
    store
        .put_if_absent(key, Bytes::from_static(b"one"))
        .await
        .unwrap();
    let op = operator(&minio, "spike");
    op.write(key, Bytes::from_static(b"two")).await.unwrap();
    store.delete(key).await.unwrap();

    let entries = op
        .list_with(key)
        .versions(true)
        .deleted(true)
        .await
        .unwrap();
    let ours: Vec<_> = entries
        .iter()
        .filter(|e| e.path().trim_start_matches('/') == key)
        .collect();
    assert_eq!(ours.len(), 3, "{ours:?}");
    assert_eq!(ours.iter().filter(|e| e.metadata().is_deleted()).count(), 1);
    assert!(ours.iter().all(|e| e.metadata().version().is_some()));
    assert_eq!(
        ours.iter()
            .filter(|e| e.metadata().is_current() == Some(true))
            .count(),
        1
    );

    for e in &ours {
        let v = e.metadata().version().unwrap();
        op.delete_with(key).version(v).await.unwrap();
    }
    assert!(minio.object_versions_all("spike", key).await.is_empty());
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

async fn write_versions(op: &Operator, key: &str, n: usize) {
    for i in 0..n {
        op.write(key, Bytes::from(format!("version {i}")))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn gc_purge_s3_versioned_removes_all_versions() {
    let minio = MinioServer::start().await;
    minio.create_versioned_bucket("versioned").await;
    let store = open_store(&minio, "versioned");
    let op = operator(&minio, "versioned");
    let key = "packs/aa/many";
    write_versions(&op, key, 3).await;
    store.delete(key).await.unwrap();
    assert_eq!(minio.object_versions_all("versioned", key).await.len(), 4);

    match store.purge(key).await {
        PurgeOutcome::Purged { versions, bytes } => {
            assert_eq!(versions, 3);
            assert_eq!(bytes, 3 * "version 0".len() as u64);
        }
        other => panic!("{other:?}"),
    }
    assert!(minio.object_versions_all("versioned", key).await.is_empty());
    assert!(matches!(store.purge(key).await, PurgeOutcome::NotFound));
}

fn expect_retained(outcome: PurgeOutcome, size: u64) -> Vec<RetainedVersion> {
    match outcome {
        PurgeOutcome::Retained {
            purged_versions,
            purged_bytes,
            retained,
        } => {
            assert_eq!((purged_versions, purged_bytes), (0, 0));
            assert_eq!(retained.len(), 1);
            assert_eq!(retained[0].size, size);
            assert!(retained[0].modified_ms.is_some_and(|t| t > 0));
            retained
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn gc_purge_s3_compliance_retained() {
    let minio = MinioServer::start().await;
    minio.create_locked_bucket("locked").await;
    let store = open_store(&minio, "locked");
    let key = "packs/aa/locked";
    store
        .put_if_absent(key, Bytes::from_static(b"payload"))
        .await
        .unwrap();

    let retained = expect_retained(store.purge(key).await, 7);
    let versions = minio.object_versions_all("locked", key).await;
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0].0, retained[0].version);
    assert!(!versions[0].1);
}

#[tokio::test]
async fn gc_purge_s3_marker_kept_while_locked() {
    let minio = MinioServer::start().await;
    minio.create_locked_bucket("locked-marker").await;
    let store = open_store(&minio, "locked-marker");
    let key = "packs/aa/hidden";
    store
        .put_if_absent(key, Bytes::from_static(b"payload"))
        .await
        .unwrap();
    store.delete(key).await.unwrap();

    expect_retained(store.purge(key).await, 7);
    let versions = minio.object_versions_all("locked-marker", key).await;
    assert_eq!(versions.len(), 2);
    assert_eq!(versions.iter().filter(|(_, marker)| *marker).count(), 1);
    assert!(store.list("packs/").await.unwrap().is_empty());
}

#[tokio::test]
async fn gc_purge_s3_governance_expired_purged() {
    let minio = MinioServer::start().await;
    minio.create_governance_bucket("governance").await;
    let store = open_store(&minio, "governance");
    let key = "packs/aa/governed";
    store
        .put_if_absent(key, Bytes::from_static(b"payload"))
        .await
        .unwrap();
    store.delete(key).await.unwrap();

    let retained = expect_retained(store.purge(key).await, 7);
    minio
        .clear_retention("governance", key, &retained[0].version)
        .await;
    match store.purge(key).await {
        PurgeOutcome::Purged { versions, bytes } => assert_eq!((versions, bytes), (1, 7)),
        other => panic!("{other:?}"),
    }
    assert!(
        minio
            .object_versions_all("governance", key)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn gc_purge_s3_retained_idempotent() {
    let minio = MinioServer::start().await;
    minio.create_locked_bucket("locked-twice").await;
    let store = open_store(&minio, "locked-twice");
    let key = "packs/aa/twice";
    store
        .put_if_absent(key, Bytes::from_static(b"payload"))
        .await
        .unwrap();
    store.delete(key).await.unwrap();

    expect_retained(store.purge(key).await, 7);
    let before = minio.object_versions_all("locked-twice", key).await;
    expect_retained(store.purge(key).await, 7);
    assert_eq!(minio.object_versions_all("locked-twice", key).await, before);
}

#[tokio::test]
async fn gc_purge_s3_plain_bucket() {
    let minio = MinioServer::start().await;
    minio.create_bucket("plain").await;
    let store = open_store(&minio, "plain");
    let key = "packs/aa/one";
    store
        .put_if_absent(key, Bytes::from_static(b"payload"))
        .await
        .unwrap();

    match store.purge(key).await {
        PurgeOutcome::Purged { versions, bytes } => assert_eq!((versions, bytes), (1, 7)),
        other => panic!("{other:?}"),
    }
    assert!(matches!(store.get(key).await, Err(BlobError::NotFound)));
    assert!(matches!(store.purge(key).await, PurgeOutcome::NotFound));
}

#[tokio::test]
async fn gc_purge_s3_not_found() {
    let minio = MinioServer::start().await;
    minio.create_versioned_bucket("empty-v").await;
    minio.create_bucket("empty-p").await;
    for bucket in ["empty-v", "empty-p"] {
        let store = open_store(&minio, bucket);
        assert!(matches!(
            store.purge("packs/aa/never").await,
            PurgeOutcome::NotFound
        ));
    }
}

#[tokio::test]
async fn gc_purge_s3_deleted_keys() {
    let minio = MinioServer::start().await;
    minio.create_versioned_bucket("markers").await;
    let store = open_store(&minio, "markers");
    for key in ["packs/aa/gone", "packs/aa/alive"] {
        store
            .put_if_absent(key, Bytes::from_static(b"x"))
            .await
            .unwrap();
    }
    store.delete("packs/aa/gone").await.unwrap();

    let now = now_ms();
    assert_eq!(
        store.deleted_keys("packs/", now + 60_000).await.unwrap(),
        vec!["packs/aa/gone".to_owned()]
    );
    assert!(
        store
            .deleted_keys("packs/", now - 3_600_000)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .deleted_keys("index/", now + 60_000)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn gc_purge_s3_agent_key_denied() {
    let minio = MinioServer::start().await;
    minio.create_versioned_bucket("agent").await;
    let root = open_store(&minio, "agent");
    let target = "packs/aa/target";
    root.put_if_absent(target, Bytes::from_static(b"payload"))
        .await
        .unwrap();
    root.put_if_absent("locks/probe-1", Bytes::from_static(b"candidates"))
        .await
        .unwrap();

    let (key, secret) = minio
        .create_user_with_policy("agent", &agent_minio_policy_json("agent", ""))
        .await;
    let agent = S3Store::open(
        &format!("s3:{}/agent", minio.endpoint()),
        Some(StaticCredentials {
            access_key_id: key.clone(),
            secret_access_key: secret.clone(),
        }),
    )
    .unwrap();

    assert!(matches!(
        agent.delete(target).await,
        Err(BlobError::Denied { .. })
    ));
    let version = minio.object_versions_all("agent", target).await[0]
        .0
        .clone();
    let err = operator_as(&minio, "agent", &key, &secret)
        .delete_with(target)
        .version(&version)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), opendal::ErrorKind::PermissionDenied);
    assert!(matches!(
        agent
            .put_if_absent("locks/gc-test", Bytes::from_static(b"x"))
            .await,
        Err(BlobError::Denied { .. })
    ));
    assert_eq!(minio.object_versions_all("agent", target).await.len(), 1);

    let listed = agent.list("locks/").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].key, "locks/probe-1");
    assert_eq!(
        &agent.get("locks/probe-1").await.unwrap()[..],
        b"candidates"
    );
    assert!(matches!(
        agent.get("locks/missing").await,
        Err(BlobError::NotFound)
    ));
}
