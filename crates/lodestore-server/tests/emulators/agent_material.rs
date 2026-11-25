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

pub(crate) const PASSWORD: &str = "correct horse battery";

pub(crate) async fn body(response: reqwest::Response) -> Value {
    response.json().await.expect("a JSON body")
}

pub(crate) fn sftp_draft(sshd: &SshdServer, name: &str) -> Value {
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

pub(crate) fn gcs_draft(gcs: &FakeGcsServer, bucket: &str, extra: Value) -> Value {
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

pub(crate) fn azure_draft(az: &AzuriteServer, container: &str) -> Value {
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

pub(crate) async fn create(api: &Api, draft: &Value) -> String {
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

pub(crate) async fn stored_location(ts: &TestServer, id: &str) -> Value {
    let row = sqlx::query("SELECT location_json FROM repositories WHERE id = $1")
        .bind(id)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap();
    serde_json::from_str(&row.get::<String, _>("location_json")).unwrap()
}

pub(crate) fn check_row<'a>(result: &'a Value, id: &str) -> &'a Value {
    result["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap()
}

pub(crate) fn make_tree(root: &Path) {
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
    let refused = api.post("/api/v1/repositories/wizard", &unpinned).await;
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

/// Asserts that `secret` reached neither the stored location nor the
/// repository detail, while both the agent and the server still receive it.
async fn assert_sealed(ts: &TestServer, api: &Api, id: &str, option: &str, secret: &str) {
    let stored = stored_location(ts, id).await;
    assert!(stored.get(option).is_none(), "{stored}");
    let row = sqlx::query("SELECT location_json FROM repositories WHERE id = $1")
        .bind(id)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap();
    assert!(!row.get::<String, _>("location_json").contains(secret));
    for path in [
        format!("/api/v1/repositories/{id}"),
        "/api/v1/repositories".to_string(),
    ] {
        let response = api.get(&path).await;
        assert_eq!(response.status(), 200);
        let text = response.text().await.unwrap();
        assert!(!text.contains(secret), "{path}: {text}");
    }
    let material = writer_material(ts.state(), id).await.unwrap();
    assert_eq!(material.location.options[option], secret);
    let access = server_access(ts.state(), id).await.unwrap();
    assert_eq!(access.location.options[option], secret);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn agent_material_gcs_token_is_sealed() {
    let gcs = FakeGcsServer::start().await;
    gcs.create_bucket("sealed").await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;

    let token = "ya29.SECRET-GCS-ACCESS-TOKEN";
    let id = create(&api, &gcs_draft(&gcs, "sealed", json!({ "token": token }))).await;
    assert_sealed(&ts, &api, &id, "token", token).await;
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn agent_material_shared_credentials_cap_the_level() {
    let az = AzuriteServer::start().await;
    az.create_container("shared").await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;

    let full_sas = az.container_sas("shared", "racwdl", std::time::Duration::from_secs(3600));
    let mut draft = azure_draft(&az, "shared");
    let options = draft["location"]["options"].as_object_mut().unwrap();
    options.remove("account_key");
    options.insert("sas_token".into(), json!(full_sas));
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
    assert_eq!(material.location.options["sas_token"], full_sas);
    assert!(!material.location.options.contains_key("account_key"));
    assert!(material.sas_expires_at_ms.is_none());

    let mut keyless = draft.clone();
    keyless["location"]["options"]
        .as_object_mut()
        .unwrap()
        .remove("sas_token");
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

pub(crate) const HOUR: i64 = 3_600_000;

pub(crate) async fn alerts_of(
    ts: &TestServer,
    kind: &str,
    state: &str,
) -> Vec<(String, String, Value)> {
    sqlx::query("SELECT key, severity, facts_json FROM alerts WHERE type = $1 AND state = $2")
        .bind(kind)
        .bind(state)
        .fetch_all(&ts.state().db.pool)
        .await
        .unwrap()
        .iter()
        .map(|row| {
            (
                row.get("key"),
                row.get("severity"),
                serde_json::from_str(&row.get::<String, _>("facts_json")).unwrap(),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn host_key_changed_raises_its_own_alert_and_clears_on_the_pinned_key() {
    let sshd = SshdServer::start().await;
    let ts = TestServer::start_with(&Backend::Sqlite, |c| c.schedule_tick_secs = 0).await;
    let api = ts.api().await;
    let id = create(&api, &sftp_draft(&sshd, "sftp-repo")).await;
    let now = lodestore_server::db::now_ms();
    let stop = tokio_util::sync::CancellationToken::new();

    sshd.rotate_host_key();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    for hours in [25, 50, 75] {
        let ran =
            lodestore_server::maintenance::guarantee::tick(ts.state(), now + hours * HOUR, &stop)
                .await
                .unwrap();
        assert_eq!(ran, std::slice::from_ref(&id));
        lodestore_server::alerts::tick(ts.state(), now + hours * HOUR)
            .await
            .unwrap();
    }
    let firing = alerts_of(&ts, "host_key_changed", "firing").await;
    assert_eq!(firing.len(), 1);
    let (key, severity, facts) = &firing[0];
    assert_eq!(key, &format!("host_key_changed:{id}"));
    assert_eq!(severity, "critical");
    let (pinned, offered) = (
        facts["pinned_fingerprint"].as_str().unwrap(),
        facts["offered_fingerprint"].as_str().unwrap(),
    );
    assert!(pinned.starts_with("SHA256:") && offered.starts_with("SHA256:"));
    assert_ne!(pinned, offered);
    assert!(
        alerts_of(&ts, "repository_unavailable", "firing")
            .await
            .is_empty()
    );
    assert!(
        alerts_of(&ts, "guarantee_downgraded", "firing")
            .await
            .is_empty()
    );
    let runs: i64 = sqlx::query("SELECT guarantee_unchecked_runs FROM repositories WHERE id = $1")
        .bind(&id)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .get(0);
    assert_eq!(runs, 3);

    let mut location = stored_location(&ts, &id).await;
    location["host_key"] = json!(sshd.host_key());
    sqlx::query("UPDATE repositories SET location_json = $1 WHERE id = $2")
        .bind(location.to_string())
        .bind(&id)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
    lodestore_server::maintenance::guarantee::tick(ts.state(), now + 100 * HOUR, &stop)
        .await
        .unwrap();
    lodestore_server::alerts::tick(ts.state(), now + 100 * HOUR)
        .await
        .unwrap();
    let mismatch: Option<String> =
        sqlx::query("SELECT host_key_mismatch_json FROM repositories WHERE id = $1")
            .bind(&id)
            .fetch_one(&ts.state().db.pool)
            .await
            .unwrap()
            .get(0);
    assert_eq!(mismatch, None);
    assert!(
        alerts_of(&ts, "host_key_changed", "firing")
            .await
            .is_empty()
    );
    assert_eq!(
        alerts_of(&ts, "host_key_changed", "resolved").await.len(),
        1
    );
    ts.stop().await;
}

pub(crate) async fn detail(api: &Api, id: &str) -> Value {
    let response = api.get(&format!("/api/v1/repositories/{id}")).await;
    assert_eq!(response.status(), 200);
    body(response).await
}

pub(crate) fn note_list(detail: &Value) -> Vec<&str> {
    detail["guarantee_notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_str().unwrap())
        .collect()
}

pub(crate) async fn vends(ts: &TestServer, id: &str) -> i64 {
    sqlx::query("SELECT agent_sas_vending FROM repositories WHERE id = $1")
        .bind(id)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .get(0)
}

/// A row as the migration leaves the repositories that existed before it.
pub(crate) async fn make_legacy(ts: &TestServer, id: &str) {
    sqlx::query("UPDATE repositories SET agent_sas_vending = 0 WHERE id = $1")
        .bind(id)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
}

pub(crate) fn named(mut draft: Value, name: &str, container: &str) -> Value {
    draft["name"] = json!(name);
    draft["location"]["options"]["container"] = json!(container);
    draft
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sas_vending_new_azblob_repository_is_create_only() {
    let az = AzuriteServer::start().await;
    az.create_container("vended").await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;

    let draft = azure_draft(&az, "vended");
    let checked = api.post("/api/v1/repositories/check", &draft).await;
    assert_eq!(checked.status(), 200);
    let text = checked.text().await.unwrap();
    assert!(!text.contains(az.account_key()), "{text}");
    let checked: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        checked["guarantee_level"],
        GuaranteeLevel::CreateOnlyEnforced.as_str(),
        "{checked}"
    );
    assert_eq!(note_list(&checked), ["lifecycle_unverified"], "{checked}");

    let id = create(&api, &draft).await;
    assert_eq!(vends(&ts, &id).await, 1);
    let shown = detail(&api, &id).await;
    assert_eq!(
        shown["guarantee_level"],
        GuaranteeLevel::CreateOnlyEnforced.as_str()
    );
    assert!(!shown.to_string().contains(az.account_key()), "{shown}");
    let list = api.get("/api/v1/repositories").await.text().await.unwrap();
    assert!(!list.contains(az.account_key()), "{list}");

    let material = writer_material(ts.state(), &id).await.unwrap();
    let options = &material.location.options;
    assert!(options["sas_token"].contains("sp=rcl"), "{options:?}");
    assert!(!options.contains_key("account_key"), "{options:?}");
    let access = server_access(ts.state(), &id).await.unwrap();
    assert_eq!(access.location.options["account_key"], az.account_key());
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sas_vending_connect_never_vends() {
    let az = AzuriteServer::start().await;
    az.create_container("again").await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;

    let first = create(&api, &azure_draft(&az, "again")).await;
    assert_eq!(vends(&ts, &first).await, 1);

    let mut second = named(azure_draft(&az, "again"), "azure-again", "again");
    let refused = api
        .post(
            "/api/v1/repositories/connect",
            &json!({ "draft": second, "password": PASSWORD }),
        )
        .await;
    assert_eq!(refused.status(), 409);
    let refused = body(refused).await;
    assert_eq!(refused["code"], "weaker_guarantee", "{refused}");
    assert_eq!(
        refused["check"]["guarantee_level"],
        GuaranteeLevel::HostCanOverwriteOrDelete.as_str()
    );

    second["accept_weaker_guarantee"] = json!(true);
    let connected = api
        .post(
            "/api/v1/repositories/connect",
            &json!({ "draft": second, "password": PASSWORD }),
        )
        .await;
    assert_eq!(connected.status(), 201, "{}", body(connected).await);
    let connected = body(connected).await;
    let id = connected["id"].as_str().unwrap();
    assert_eq!(vends(&ts, id).await, 0);
    let shown = detail(&api, id).await;
    assert_eq!(
        shown["guarantee_level"],
        GuaranteeLevel::HostCanOverwriteOrDelete.as_str()
    );
    assert!(note_list(&shown).contains(&"shared_credentials"), "{shown}");
    let material = writer_material(ts.state(), id).await.unwrap();
    assert_eq!(material.location.options["account_key"], az.account_key());
    assert!(material.sas_expires_at_ms.is_none());
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sas_vending_existing_repository_keeps_shared_credentials() {
    let az = AzuriteServer::start().await;
    az.create_container("old").await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;

    let id = create(&api, &azure_draft(&az, "old")).await;
    make_legacy(&ts, &id).await;
    let rechecked = api
        .post(&format!("/api/v1/repositories/{id}/check"), &json!({}))
        .await;
    assert_eq!(rechecked.status(), 200);
    let rechecked = body(rechecked).await;
    assert_eq!(
        rechecked["guarantee_level"],
        GuaranteeLevel::HostCanOverwriteOrDelete.as_str(),
        "{rechecked}"
    );
    assert!(
        rechecked["guarantee_notes"]
            .as_array()
            .unwrap()
            .contains(&json!("shared_credentials")),
        "{rechecked}"
    );
    let material = writer_material(ts.state(), &id).await.unwrap();
    assert_eq!(material.location.options["account_key"], az.account_key());
    assert!(!material.location.options.contains_key("sas_token"));
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sas_vending_daily_recheck_measures_the_vended_sas() {
    let az = AzuriteServer::start().await;
    az.create_container("daily").await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;

    let id = create(&api, &azure_draft(&az, "daily")).await;
    let result = lodestore_server::repos::agent_key::recheck_repository_at(
        ts.state(),
        &id,
        lodestore_server::db::now_ms(),
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        result.guarantee_level,
        GuaranteeLevel::CreateOnlyEnforced.as_str()
    );
    assert!(
        !result
            .guarantee_notes
            .contains(&"shared_credentials".to_string())
    );
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sas_vending_refuses_agent_options() {
    let az = AzuriteServer::start().await;
    az.create_container("noopts").await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;

    let id = create(&api, &azure_draft(&az, "noopts")).await;
    let sas = az.container_sas("noopts", "rcl", std::time::Duration::from_secs(3600));
    let response = api
        .put(
            &format!("/api/v1/repositories/{id}/agent-options"),
            &json!({ "agent_options": { "sas_token": sas } }),
        )
        .await;
    assert_eq!(response.status(), 409);
    let refused = body(response).await;
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("short-lived SAS tokens signed by the server"),
        "{refused}"
    );
    ts.stop().await;
}
