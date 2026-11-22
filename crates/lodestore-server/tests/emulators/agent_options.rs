use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::agent_material::{PASSWORD, azure_draft, body, create, gcs_draft, sftp_draft};
use crate::common::{Backend, TestServer};
use lodestore_format::store::probe::GuaranteeLevel;
use lodestore_format::store::{BlobStore, RepoLocation};
use lodestore_server::repos::db::{server_access, writer_material};
use lodestore_testkit::azurite::AzuriteServer;
use lodestore_testkit::gcs::FakeGcsServer;
use lodestore_testkit::sshd::{SshUser, SshdServer};
use serde_json::{Value, json};
use sqlx::Row;

fn notes(result: &Value) -> Vec<&str> {
    result["guarantee_notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_str().unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn agent_options_sftp_restricted_user_is_measured_by_its_own_rights() {
    let sshd = SshdServer::start().await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;

    let srv_draft = sftp_draft(&sshd, "prefixes");
    let srv = RepoLocation::new(
        "sftp",
        srv_draft["location"]["options"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str().unwrap())),
    )
    .open(None)
    .unwrap();
    srv.ensure_prefixes(&["packs", "index", "snapshots"])
        .await
        .unwrap();

    let mut draft = sftp_draft(&sshd, "sftp-own-rights");
    draft["agent_options"] = json!({
        "user": SshUser::Agent.login(),
        "key": sshd.private_key(SshUser::Agent),
    });

    let checked = api.post("/api/v1/repositories/check", &draft).await;
    assert_eq!(checked.status(), 200);
    let checked = body(checked).await;
    let found = notes(&checked);
    assert!(found.contains(&"held_handle_rewrite"), "{checked}");
    assert!(!found.contains(&"shared_credentials"), "{checked}");
    assert_eq!(
        checked["guarantee_level"],
        GuaranteeLevel::HostCanOverwriteOrDelete.as_str()
    );

    let id = create(&api, &draft).await;

    let material = writer_material(ts.state(), &id).await.unwrap();
    assert_eq!(material.location.options["user"], SshUser::Agent.login());
    assert_eq!(
        material.location.options["key"],
        sshd.private_key(SshUser::Agent)
    );
    let access = server_access(ts.state(), &id).await.unwrap();
    assert_eq!(access.location.options["user"], SshUser::Srv.login());
    assert_eq!(
        access.location.options["key"],
        sshd.private_key(SshUser::Srv)
    );
    assert_eq!(
        access.agent_options["user"].as_str(),
        SshUser::Agent.login()
    );

    let rechecked = api
        .post(&format!("/api/v1/repositories/{id}/check"), &json!({}))
        .await;
    assert_eq!(rechecked.status(), 200);
    let rechecked = body(rechecked).await;
    let found = notes(&rechecked);
    assert!(found.contains(&"held_handle_rewrite"), "{rechecked}");
    assert!(!found.contains(&"shared_credentials"), "{rechecked}");
    ts.stop().await;
}

fn sas_signature(sas: &str) -> &str {
    sas.split('&')
        .find_map(|part| part.strip_prefix("sig="))
        .expect("a signature in the SAS")
}

fn azure_sas_draft(az: &AzuriteServer, container: &str, name: &str, sas: &str) -> Value {
    let mut draft = azure_draft(az, container);
    draft["name"] = json!(name);
    draft["agent_options"] = json!({ "sas_token": sas });
    draft
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn agent_options_azblob_rcl_sas_is_create_only() {
    let az = AzuriteServer::start().await;
    az.create_container("rcl").await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;

    let sas = az.container_sas("rcl", "rcl", Duration::from_secs(3600));
    let draft = azure_sas_draft(&az, "rcl", "azure-rcl", &sas);

    let checked = api.post("/api/v1/repositories/check", &draft).await;
    assert_eq!(checked.status(), 200);
    let checked = body(checked).await;
    assert_eq!(
        checked["guarantee_level"],
        GuaranteeLevel::CreateOnlyEnforced.as_str(),
        "{checked}"
    );
    assert_eq!(notes(&checked), ["lifecycle_unverified"], "{checked}");

    let id = create(&api, &draft).await;
    let material = writer_material(ts.state(), &id).await.unwrap();
    assert_eq!(material.location.options["sas_token"], sas);
    assert!(!material.location.options.contains_key("account_key"));
    let access = server_access(ts.state(), &id).await.unwrap();
    assert_eq!(access.location.options["account_key"], az.account_key());
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn gcs_level_carries_lifecycle_unverified() {
    let gcs = FakeGcsServer::start().await;
    gcs.create_bucket("lifecycle").await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;

    let draft = gcs_draft(&gcs, "lifecycle", json!({ "token": "ya29.SECRET" }));
    let checked = api.post("/api/v1/repositories/check", &draft).await;
    assert_eq!(checked.status(), 200);
    let checked = body(checked).await;
    let found = notes(&checked);
    assert!(found.contains(&"lifecycle_unverified"), "{checked}");
    assert!(found.contains(&"shared_credentials"), "{checked}");
    ts.stop().await;
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogBuffer {
    fn take(&self) -> String {
        String::from_utf8_lossy(&std::mem::take(&mut *self.0.lock().unwrap())).into_owned()
    }
}

fn capture_logs() -> LogBuffer {
    let logs = LogBuffer::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("the only test that logs");
    logs
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn agent_options_are_never_echoed() {
    let logs = capture_logs();
    let az = AzuriteServer::start().await;
    for container in ["echo", "echoweak", "echoread"] {
        az.create_container(container).await;
    }
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;
    let ttl = Duration::from_secs(3600);

    let strong = az.container_sas("echo", "rcl", ttl);
    let weak = az.container_sas("echoweak", "racwdl", ttl);
    let read_only = az.container_sas("echoread", "r", ttl);
    let signatures = [&strong, &weak, &read_only].map(|s| sas_signature(s).to_string());
    let mut seen = String::new();

    let draft = azure_sas_draft(&az, "echo", "echo-repo", &strong);
    let checked = api.post("/api/v1/repositories/check", &draft).await;
    assert_eq!(checked.status(), 200);
    seen.push_str(&checked.text().await.unwrap());

    let weaker = azure_sas_draft(&az, "echoweak", "echo-weak", &weak);
    let refused = api.post("/api/v1/repositories/wizard", &weaker).await;
    assert_eq!(refused.status(), 409);
    let refused = refused.text().await.unwrap();
    assert!(refused.contains("weaker_guarantee"), "{refused}");
    seen.push_str(&refused);

    let unreadable = azure_sas_draft(&az, "echoread", "echo-read", &read_only);
    let failed = api.post("/api/v1/repositories/check", &unreadable).await;
    let failed = failed.text().await.unwrap();
    eprintln!("read-only SAS check: {failed}");
    seen.push_str(&failed);

    let started = api.post("/api/v1/repositories/wizard", &draft).await;
    assert_eq!(started.status(), 201);
    let started = body(started).await;
    seen.push_str(&started.to_string());
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
    assert_eq!(committed.status(), 201);
    let committed = body(committed).await;
    seen.push_str(&committed.to_string());
    let id = committed["id"].as_str().unwrap().to_string();

    for path in [
        format!("/api/v1/repositories/{id}"),
        "/api/v1/repositories".to_string(),
    ] {
        let response = api.get(&path).await;
        assert_eq!(response.status(), 200);
        seen.push_str(&response.text().await.unwrap());
    }
    let rechecked = api
        .post(&format!("/api/v1/repositories/{id}/check"), &json!({}))
        .await;
    assert_eq!(rechecked.status(), 200);
    seen.push_str(&rechecked.text().await.unwrap());

    let row = sqlx::query("SELECT location_json, checks_json FROM repositories WHERE id = $1")
        .bind(&id)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap();
    seen.push_str(&row.get::<String, _>("location_json"));
    seen.push_str(
        &row.get::<Option<String>, _>("checks_json")
            .unwrap_or_default(),
    );
    let secrets = sqlx::query("SELECT kind, ciphertext FROM secrets")
        .fetch_all(&ts.state().db.pool)
        .await
        .unwrap();
    assert!(
        secrets
            .iter()
            .any(|r| r.get::<String, _>("kind") == "repo-agent-creds")
    );
    for secret in &secrets {
        let bytes: Vec<u8> = secret.get("ciphertext");
        for signature in &signatures {
            assert!(
                !bytes
                    .windows(signature.len())
                    .any(|w| w == signature.as_bytes()),
                "a signature is in a sealed secret as plain text"
            );
        }
    }

    let log = logs.take();
    for signature in &signatures {
        assert!(
            !seen.contains(signature.as_str()),
            "a SAS signature reached a response or a column"
        );
        assert!(
            !log.contains(signature.as_str()),
            "a SAS signature reached the server log"
        );
    }
    ts.stop().await;
}
