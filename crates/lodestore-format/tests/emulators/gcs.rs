use lodestore_format::store::{RepoLocation, Store};
use lodestore_testkit::gcs::FakeGcsServer;

fn store(gcs: &FakeGcsServer, bucket: &str) -> Store {
    RepoLocation::new(
        "gcs",
        [
            ("bucket", bucket),
            ("endpoint", gcs.endpoint().as_str()),
            ("skip_signature", "true"),
            ("disable_vm_metadata", "true"),
            ("disable_config_load", "true"),
        ],
    )
    .open(None)
    .unwrap()
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn gcs_put_twice_is_already_exists() {
    let gcs = FakeGcsServer::start().await;
    gcs.create_bucket("twice").await;
    super::put_twice_is_already_exists(&store(&gcs, "twice")).await;
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn gcs_backup_and_restore() {
    let gcs = FakeGcsServer::start().await;
    gcs.create_bucket("cycle").await;
    super::full_cycle(&store(&gcs, "cycle")).await;
}
