use std::path::Path;

use crate::common::{Api, Backend, TestServer};
use lodestore_format::keys::WriteKeys;
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::repo::Repository;
use lodestore_format::store::RepoLocation;
use lodestore_format::store::probe::GuaranteeLevel;
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_server::repos::db::{server_access, writer_material};
use lodestore_testkit::azurite::AzuriteServer;
use lodestore_testkit::gcs::FakeGcsServer;
use lodestore_testkit::sshd::{SshUser, SshdServer};
use serde_json::{Value, json};
use sqlx::Row;

const PASSWORD: &str = "correct horse battery";

async fn body(response: reqwest::Response) -> Value {
    response.json().await.expect("a JSON body")
}

fn sftp_draft(sshd: &SshdServer, name: &str) -> Value {
    json!({
        "name": name,
        "location": { "backend": "sftp", "options": {
            "endpoint": sshd.endpoint(),
            "user": SshUser::Srv.login(),
            "key": sshd.private_key(SshUser::Srv),
            "host_key": sshd.host_key(),
            "root": sshd.root(),
        } },
        "accept_weaker_guarantee": true,
    })
}

fn gcs_draft(gcs: &FakeGcsServer, bucket: &str, extra: Value) -> Value {
    let mut options = json!({
        "bucket": bucket,
        "endpoint": gcs.endpoint(),
        "disable_vm_metadata": "true",
        "disable_config_load": "true",
    });
    options
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    json!({
        "name": "gcs-repo",
        "location": { "backend": "gcs", "options": options },
        "accept_weaker_guarantee": true,
    })
}

fn azure_draft(az: &AzuriteServer, container: &str) -> Value {
    json!({
        "name": "azure-repo",
        "location": { "backend": "azblob", "options": {
            "container": container,
            "endpoint": az.endpoint(),
            "account_name": az.account_name(),
            "account_key": az.account_key(),
        } },
    })
}

async fn create(api: &Api, draft: &Value) -> String {
    let started = api.post("/api/v1/repositories/wizard", draft).await;
    assert_eq!(started.status(), 201, "{}", body(started).await);
    let started = body(started).await;
    let last_group = started["recovery_key"]
        .as_str()
        .unwrap()
        .rsplit('-')
        .next()
        .unwrap()
        .to_string();
    let committed = api
        .post(
            &format!(
                "/api/v1/repositories/wizard/{}/commit",
                started["wizard_id"].as_str().unwrap()
            ),
            &json!({ "password": PASSWORD, "recovery_key_last_group": last_group }),
        )
        .await;
    assert_eq!(committed.status(), 201, "{}", body(committed).await);
    body(committed).await["id"].as_str().unwrap().to_string()
}

async fn stored_location(ts: &TestServer, id: &str) -> Value {
    let row = sqlx::query("SELECT location_json FROM repositories WHERE id = $1")
        .bind(id)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap();
    serde_json::from_str(&row.get::<String, _>("location_json")).unwrap()
}

fn check_row<'a>(result: &'a Value, id: &str) -> &'a Value {
    result["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap()
}

fn make_tree(root: &Path) {
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("big.bin"), vec![7u8; 3 << 20]).unwrap();
    std::fs::write(root.join("sub/small.txt"), b"hello agent material").unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn agent_material_sftp_host_key_reaches_the_agent() {
    let sshd = SshdServer::start().await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;

    let mut unpinned = sftp_draft(&sshd, "sftp-repo");
    unpinned["location"]["options"]
        .as_object_mut()
        .unwrap()
        .remove("host_key");
    let refused = api.post("/api/v1/repositories/check", &unpinned).await;
    assert_eq!(refused.status(), 422);
    let refused = body(refused).await;
    assert!(
        refused["message"].as_str().unwrap().contains("ssh-keyscan"),
        "{refused}"
    );

    let mut draft = sftp_draft(&sshd, "sftp-repo");
    draft["accept_weaker_guarantee"] = json!(false);
    let refused = api.post("/api/v1/repositories/wizard", &draft).await;
    assert_eq!(refused.status(), 409);
    assert_eq!(body(refused).await["code"], "weaker_guarantee");

    draft["accept_weaker_guarantee"] = json!(true);
    let id = create(&api, &draft).await;

    let stored = stored_location(&ts, &id).await;
    assert_eq!(stored["host_key"], sshd.host_key());
    assert!(stored.get("key").is_none(), "{stored}");

    let material = writer_material(ts.state(), &id).await.unwrap();
    assert_eq!(material.location.options["host_key"], sshd.host_key());
    assert_eq!(
        material.location.options["key"],
        sshd.private_key(SshUser::Srv)
    );

    let agent_store = RepoLocation {
        backend: material.location.backend.clone(),
        options: material.location.options.clone(),
    }
    .open(None)
    .unwrap();
    let repo = Repository::open(&agent_store, PASSWORD.as_bytes())
        .await
        .unwrap();
    let keys = WriteKeys::from_file_text(&material.writer_keys_text).unwrap();
    let mut writer = Writer::new(&agent_store, keys, repo.config())
        .await
        .unwrap();
    let src = tempfile::tempdir().unwrap();
    make_tree(src.path());
    let outcome = writer
        .backup(
            &[src.path().to_path_buf()],
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert!(!outcome.incomplete);

    let access = server_access(ts.state(), &id).await.unwrap();
    let server_store = access.location.open(None).unwrap();
    let repo = Repository::open(&server_store, PASSWORD.as_bytes())
        .await
        .unwrap();
    let mut reader =
        Reader::open(&server_store, repo.read_keys().duplicate(), repo.config()).unwrap();
    let out = tempfile::tempdir().unwrap();
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
    let restored = out.path().join(canonical.strip_prefix("/").unwrap());
    assert_eq!(
        std::fs::read(restored.join("sub/small.txt")).unwrap(),
        b"hello agent material"
    );
    assert_eq!(
        std::fs::read(restored.join("big.bin")).unwrap(),
        std::fs::read(src.path().join("big.bin")).unwrap()
    );
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn agent_material_gcs_never_uses_ambient_credentials() {
    let gcs = FakeGcsServer::start().await;
    gcs.create_bucket("agent-material").await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;

    let path_only = gcs_draft(
        &gcs,
        "agent-material",
        json!({ "credential_path": "/etc/host-credentials.json" }),
    );
    let refused = api.post("/api/v1/repositories/check", &path_only).await;
    assert_eq!(refused.status(), 422);
    let refused = body(refused).await;
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("agents cannot read credential_path"),
        "{refused}"
    );

    let draft = gcs_draft(
        &gcs,
        "agent-material",
        json!({ "credential_path": "/etc/host-credentials.json", "skip_signature": "true" }),
    );
    let id = create(&api, &draft).await;

    let material = writer_material(ts.state(), &id).await.unwrap();
    let options = &material.location.options;
    assert!(!options.contains_key("credential_path"), "{options:?}");
    assert_eq!(options["disable_vm_metadata"], "true");
    assert_eq!(options["disable_config_load"], "true");
    let access = server_access(ts.state(), &id).await.unwrap();
    assert_eq!(
        access.location.options["credential_path"],
        "/etc/host-credentials.json"
    );
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn agent_material_shared_credentials_cap_the_level() {
    let az = AzuriteServer::start().await;
    az.create_container("shared").await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;

    let draft = azure_draft(&az, "shared");
    let checked = api.post("/api/v1/repositories/check", &draft).await;
    assert_eq!(checked.status(), 200);
    let checked = body(checked).await;
    assert_eq!(
        checked["guarantee_level"],
        GuaranteeLevel::HostCanOverwriteOrDelete.as_str()
    );
    let notes = checked["guarantee_notes"].as_array().unwrap();
    assert!(notes.contains(&json!("shared_credentials")), "{checked}");

    let refused = api.post("/api/v1/repositories/wizard", &draft).await;
    assert_eq!(refused.status(), 409);
    assert_eq!(body(refused).await["code"], "weaker_guarantee");

    let mut accepted = draft.clone();
    accepted["accept_weaker_guarantee"] = json!(true);
    let id = create(&api, &accepted).await;
    let material = writer_material(ts.state(), &id).await.unwrap();
    assert_eq!(material.location.options["account_key"], az.account_key());

    let mut keyless = draft.clone();
    keyless["location"]["options"]
        .as_object_mut()
        .unwrap()
        .remove("account_key");
    let refused = api.post("/api/v1/repositories/check", &keyless).await;
    assert_eq!(refused.status(), 422);
    assert!(
        body(refused).await["message"]
            .as_str()
            .unwrap()
            .contains("static credentials")
    );
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn agent_material_sftp_changed_host_key_is_a_distinct_error() {
    let sshd = SshdServer::start().await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;
    let id = create(&api, &sftp_draft(&sshd, "sftp-repo")).await;

    sshd.rotate_host_key();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let rechecked = api
        .post(&format!("/api/v1/repositories/{id}/check"), &json!({}))
        .await;
    assert_eq!(rechecked.status(), 200);
    let rechecked = body(rechecked).await;
    assert_eq!(rechecked["blocking"], true);
    let write = check_row(&rechecked, "write");
    assert_eq!(write["status"], "fail");
    let error = write["error"].as_str().unwrap();
    assert!(error.contains("host key changed"), "{error}");
    assert_eq!(error.matches("SHA256:").count(), 2, "{error}");
    assert!(!error.contains("temporar"), "{error}");
    assert_eq!(rechecked["guarantee_level"], "unchecked");
    ts.stop().await;
}
