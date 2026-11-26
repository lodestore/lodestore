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

/// A row as an older server (or any path around the API) leaves it: the
/// account key goes to the agents, nothing in the container itself.
pub(crate) async fn legacy_row(
    ts: &TestServer,
    az: &AzuriteServer,
    name: &str,
    container: &str,
) -> String {
    use lodestore_format::crypto::Entropy;
    use lodestore_format::repo::{InitOptions, PendingInit};
    use lodestore_server::repos::db::{NewRepository, RepoExtras, create_repository_ext};

    let pending = PendingInit::generate(&mut Entropy::os(), InitOptions::default()).unwrap();
    let keys = pending.read_keys();
    create_repository_ext(
        ts.state(),
        NewRepository {
            name: name.to_string(),
            location: lodestore_server::repos::db::RepoLocation {
                backend: "azblob".into(),
                options: [
                    ("container", container),
                    ("endpoint", &az.endpoint()),
                    ("account_name", az.account_name()),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            },
            format_repo_id: *keys.repo_id(),
            config_hash: lodestore_format::config::config_hash(pending.config_bytes()),
            master: keys.master().clone(),
            server_slot_secret: None,
            server_creds: None,
            agent_creds: None,
        },
        RepoExtras {
            secret_options: [("account_key".to_string(), az.account_key().to_string())].into(),
            ..RepoExtras::default()
        },
    )
    .await
    .unwrap()
}

async fn check_of(api: &Api, draft: &Value) -> Value {
    let checked = api.post("/api/v1/repositories/check", draft).await;
    assert_eq!(checked.status(), 200);
    body(checked).await
}

async fn recheck_of(api: &Api, id: &str) -> Value {
    let rechecked = api
        .post(&format!("/api/v1/repositories/{id}/check"), &json!({}))
        .await;
    assert_eq!(rechecked.status(), 200);
    body(rechecked).await
}

fn assert_shared(result: &Value) {
    assert_eq!(
        result["guarantee_level"],
        GuaranteeLevel::HostCanOverwriteOrDelete.as_str(),
        "{result}"
    );
    assert!(
        note_list(result).contains(&"shared_credentials"),
        "{result}"
    );
}

fn assert_create_only(result: &Value) {
    assert_eq!(
        result["guarantee_level"],
        GuaranteeLevel::CreateOnlyEnforced.as_str(),
        "{result}"
    );
    assert!(
        !note_list(result).contains(&"shared_credentials"),
        "{result}"
    );
}

fn with_sas(mut draft: Value, name: &str, sas: &str) -> Value {
    draft["name"] = json!(name);
    draft["agent_options"] = json!({ "sas_token": sas });
    draft
}

fn one_hour() -> std::time::Duration {
    std::time::Duration::from_secs(3600)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sas_vending_account_key_neighbour_caps_the_level() {
    let az = AzuriteServer::start().await;
    az.create_container("key-a").await;
    az.create_container("key-b").await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;

    legacy_row(&ts, &az, "r1", "key-a").await;
    let draft = named(azure_draft(&az, "key-b"), "r2", "key-b");
    assert_shared(&check_of(&api, &draft).await);
    let refused = api.post("/api/v1/repositories/wizard", &draft).await;
    assert_eq!(refused.status(), 409);
    assert_eq!(body(refused).await["code"], "weaker_guarantee");
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sas_vending_neighbour_added_later_caps_on_recheck() {
    let az = AzuriteServer::start().await;
    az.create_container("late-b").await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;

    let id = create(&api, &named(azure_draft(&az, "late-b"), "r2", "late-b")).await;
    assert_create_only(&recheck_of(&api, &id).await);
    legacy_row(&ts, &az, "r1", "late-a").await;
    assert_shared(&recheck_of(&api, &id).await);
    let result = lodestore_server::repos::agent_key::recheck_repository_at(
        ts.state(),
        &id,
        lodestore_server::db::now_ms(),
        true,
    )
    .await
    .unwrap();
    assert!(
        result
            .guarantee_notes
            .contains(&"shared_credentials".to_string())
    );
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sas_vending_connected_neighbour_caps_immediately() {
    let az = AzuriteServer::start().await;
    az.create_container("conn-b").await;
    az.create_container("conn-x").await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;

    let r2 = create(&api, &named(azure_draft(&az, "conn-b"), "r2", "conn-b")).await;
    create(&api, &named(azure_draft(&az, "conn-x"), "rx", "conn-x")).await;
    assert_create_only(&detail(&api, &r2).await);

    let mut draft = named(azure_draft(&az, "conn-x"), "r3", "conn-x");
    draft["accept_weaker_guarantee"] = json!(true);
    let connected = api
        .post(
            "/api/v1/repositories/connect",
            &json!({ "draft": draft, "password": PASSWORD }),
        )
        .await;
    assert_eq!(connected.status(), 201, "{}", body(connected).await);
    assert_shared(&detail(&api, &r2).await);
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sas_vending_neighbour_on_the_same_container_caps_immediately() {
    let az = AzuriteServer::start().await;
    az.create_container("same").await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;

    let r2 = create(&api, &named(azure_draft(&az, "same"), "r2", "same")).await;
    assert_create_only(&detail(&api, &r2).await);
    let mut other = named(azure_draft(&az, "same"), "r3", "same");
    other["location"]["options"]["root"] = json!("other-root");
    other["accept_weaker_guarantee"] = json!(true);
    let r3 = create(&api, &other).await;
    assert_eq!(vends(&ts, &r3).await, 0);
    assert_shared(&detail(&api, &r2).await);
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sas_vending_widened_agent_options_caps_immediately() {
    let az = AzuriteServer::start().await;
    for container in ["wide-y", "wide-z"] {
        az.create_container(container).await;
    }
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;

    let r2 = create(&api, &named(azure_draft(&az, "wide-z"), "r2", "wide-z")).await;
    let narrow = az.container_sas("wide-y", "rcl", one_hour());
    let r4 = create(
        &api,
        &with_sas(
            named(azure_draft(&az, "wide-y"), "r4", "wide-y"),
            "r4",
            &narrow,
        ),
    )
    .await;
    assert_create_only(&detail(&api, &r2).await);

    let put = |sas: String| {
        let (api, path) = (&api, format!("/api/v1/repositories/{r4}/agent-options"));
        async move {
            api.put(&path, &json!({ "agent_options": { "sas_token": sas } }))
                .await
        }
    };
    let again = az.container_sas("wide-y", "rcl", std::time::Duration::from_secs(7200));
    let narrow_put = put(again).await;
    assert_eq!(narrow_put.status(), 200, "{}", body(narrow_put).await);
    assert_create_only(&detail(&api, &r2).await);

    let broad = az.container_sas("wide-y", "racwdl", one_hour());
    let wide_put = put(broad).await;
    assert_eq!(wide_put.status(), 200, "{}", body(wide_put).await);
    assert_shared(&detail(&api, &r2).await);
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sas_vending_narrow_neighbour_keeps_create_only() {
    let az = AzuriteServer::start().await;
    for container in ["narrow-y", "narrow-z"] {
        az.create_container(container).await;
    }
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;

    let narrow = az.container_sas("narrow-y", "rcl", one_hour());
    create(
        &api,
        &with_sas(
            named(azure_draft(&az, "narrow-y"), "r4", "narrow-y"),
            "r4",
            &narrow,
        ),
    )
    .await;
    let draft = named(azure_draft(&az, "narrow-z"), "r2", "narrow-z");
    assert_create_only(&check_of(&api, &draft).await);
    let r2 = create(&api, &draft).await;
    assert_create_only(&recheck_of(&api, &r2).await);
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sas_vending_second_repository_on_the_container_does_not_vend() {
    let az = AzuriteServer::start().await;
    az.create_container("shared-c").await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;

    let mut first = named(azure_draft(&az, "shared-c"), "r1", "shared-c");
    first["location"]["options"]["root"] = json!("a");
    let r1 = create(&api, &first).await;
    assert_eq!(vends(&ts, &r1).await, 1);

    let mut second = named(azure_draft(&az, "shared-c"), "r2", "shared-c");
    second["location"]["options"]["root"] = json!("b");
    assert_shared(&check_of(&api, &second).await);
    second["accept_weaker_guarantee"] = json!(true);
    let r2 = create(&api, &second).await;
    assert_eq!(vends(&ts, &r2).await, 0);
    let material = writer_material(ts.state(), &r2).await.unwrap();
    assert_eq!(material.location.options["account_key"], az.account_key());
    assert_shared(&recheck_of(&api, &r1).await);
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sas_vending_commit_refuses_when_the_container_was_taken() {
    let az = AzuriteServer::start().await;
    az.create_container("taken").await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;

    let draft = named(azure_draft(&az, "taken"), "r2", "taken");
    let started = api.post("/api/v1/repositories/wizard", &draft).await;
    assert_eq!(started.status(), 201);
    let started = body(started).await;
    legacy_row(&ts, &az, "r1", "taken").await;

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
    assert_eq!(committed.status(), 409);
    assert_eq!(body(committed).await["code"], "storage_changed");
    let rows: i64 = sqlx::query("SELECT COUNT(*) FROM repositories WHERE name = 'r2'")
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .get(0);
    assert_eq!(rows, 0);
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn static_sas_repository_with_an_account_key_neighbour_is_capped() {
    let az = AzuriteServer::start().await;
    az.create_container("static-y").await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;

    let narrow = az.container_sas("static-y", "rcl", one_hour());
    let draft = with_sas(
        named(azure_draft(&az, "static-y"), "r4", "static-y"),
        "r4",
        &narrow,
    );
    let r4 = create(&api, &draft).await;
    assert_create_only(&recheck_of(&api, &r4).await);
    legacy_row(&ts, &az, "r1", "static-other").await;
    assert_shared(&recheck_of(&api, &r4).await);
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sas_vending_refreshing_store_probes_like_the_server() {
    use lodestore_format::store::probe::{AgentProbe, ProbeKeys, probe_keep, remove_probe_objects};
    use lodestore_server::repos::checks::open_parts_with;
    use zeroize::Zeroizing;

    let az = AzuriteServer::start().await;
    az.create_container("refreshing").await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;

    let id = create(&api, &azure_draft(&az, "refreshing")).await;
    assert_eq!(vends(&ts, &id).await, 1);
    let access = server_access(ts.state(), &id).await.unwrap();
    assert!(access.sas_vending);
    let opened = open_parts_with(
        access.location,
        access.server_creds,
        access.agent_creds,
        access.agent_options,
        access.sas_vending,
    )
    .unwrap();
    let server_agent = opened.agent.as_ref().expect("the agent store");
    let keys_a = ProbeKeys::default();
    let by_server = probe_keep(&opened.server, AgentProbe::Store(server_agent), &keys_a).await;
    assert!(
        remove_probe_objects(&opened.server, &keys_a)
            .await
            .is_empty()
    );

    let material = writer_material(ts.state(), &id).await.unwrap();
    let sas = Zeroizing::new(material.location.options["sas_token"].clone());
    assert!(
        sas.contains("sp=rcl"),
        "the material carries the vended SAS"
    );
    assert!(!material.location.options.contains_key("account_key"));
    let source: lodestore_format::store::opendal::SasSource =
        std::sync::Arc::new(move || Some(sas.clone()));
    let agent_store = RepoLocation {
        backend: material.location.backend.clone(),
        options: material.location.options.clone(),
    }
    .for_agent()
    .open_refreshing(source, std::time::Duration::from_secs(1))
    .unwrap();
    let keys_b = ProbeKeys::default();
    let by_agent = probe_keep(&opened.server, AgentProbe::Store(&agent_store), &keys_b).await;
    assert!(
        remove_probe_objects(&opened.server, &keys_b)
            .await
            .is_empty()
    );

    assert_eq!(by_server.guarantee, GuaranteeLevel::CreateOnlyEnforced);
    assert_eq!(by_agent.guarantee, by_server.guarantee);
    for (agent, server) in by_agent.checks.iter().zip(&by_server.checks) {
        assert_eq!((agent.id, agent.status), (server.id, server.status));
    }
    assert_eq!(by_agent.weaknesses, by_server.weaknesses);
    assert_eq!(by_agent.agent_key, by_server.agent_key);
    ts.stop().await;
}
