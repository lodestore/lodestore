use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::agent_material::{PASSWORD, azure_draft, body, create, gcs_draft, sftp_draft};
use crate::common::{Api, Backend, TestServer, WAIT, wait_until};
use crate::host_key_push::{
    agent_id, capabilities_vec, enroll_agent, open_session, open_session_at,
};
use lodestore_agent::{NetOptions, Paths, RunArgs};
use lodestore_format::store::probe::GuaranteeLevel;
use lodestore_format::store::{BlobStore, RepoLocation};
use lodestore_proto::agent::v1::ServerMessage;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_server::repos::db::{server_access, writer_material};
use lodestore_testkit::azurite::AzuriteServer;
use lodestore_testkit::fault_proxy::{FaultPlan, FaultProxy};
use lodestore_testkit::gcs::FakeGcsServer;
use lodestore_testkit::sshd::{SshUser, SshdServer};
use serde_json::{Value, json};
use sqlx::Row;
use tokio_util::sync::CancellationToken;

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

const TTL: Duration = Duration::from_secs(3600);

fn sas_with(az: &AzuriteServer, container: &str, permissions: &str, minutes: u64) -> String {
    az.container_sas(
        container,
        permissions,
        TTL + Duration::from_secs(minutes * 60),
    )
}

async fn azure_repo(
    ts: &TestServer,
    az: &AzuriteServer,
    container: &str,
    sas: &str,
    accept_weaker: bool,
) -> (Arc<Api>, String) {
    let api = Arc::new(ts.api().await);
    let mut draft = azure_sas_draft(az, container, "azure-rotation", sas);
    draft["accept_weaker_guarantee"] = json!(accept_weaker);
    let id = create(&api, &draft).await;
    (api, id)
}

fn options_path(id: &str) -> String {
    format!("/api/v1/repositories/{id}/agent-options")
}

async fn put_options(api: &Api, id: &str, options: Value) -> (u16, Value) {
    let response = api
        .put(&options_path(id), &json!({ "agent_options": options }))
        .await;
    (response.status().as_u16(), body(response).await)
}

fn put_sas(api: &Arc<Api>, id: &str, sas: &str) -> tokio::task::JoinHandle<(u16, Value)> {
    let (api, id, sas) = (Arc::clone(api), id.to_string(), sas.to_string());
    tokio::spawn(async move { put_options(&api, &id, json!({ "sas_token": sas })).await })
}

async fn marks(ts: &TestServer, id: &str) -> sqlx::any::AnyRow {
    sqlx::query(
        "SELECT access_generation, agent_creds_secret_id, guarantee_level, guarantee_accepted, \
         guarantee_notes_json, agent_key_state, checks_json, guarantee_unchecked_runs, \
         guarantee_failed_runs FROM repositories WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&ts.state().db.pool)
    .await
    .unwrap()
}

fn generation(row: &sqlx::any::AnyRow) -> i64 {
    row.get("access_generation")
}

async fn agent_sas(ts: &TestServer, id: &str) -> String {
    writer_material(ts.state(), id)
        .await
        .unwrap()
        .location
        .options["sas_token"]
        .clone()
}

fn location_of(message: ServerMessage) -> lodestore_proto::agent::v1::RepoLocation {
    match message.body {
        Some(ServerBody::UpdateLocation(update)) => update.location.expect("a location"),
        other => panic!("expected UpdateLocation, got {other:?}"),
    }
}

async fn next_location(
    stream: &mut tonic::Streaming<ServerMessage>,
) -> lodestore_proto::agent::v1::RepoLocation {
    let next = tokio::time::timeout(Duration::from_secs(10), stream.message())
        .await
        .expect("an UpdateLocation arrives")
        .unwrap()
        .unwrap();
    location_of(next)
}

async fn silent_of_locations(stream: &mut tonic::Streaming<ServerMessage>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while let Ok(Ok(Some(message))) = tokio::time::timeout_at(deadline, stream.message()).await {
        assert!(
            !matches!(message.body, Some(ServerBody::UpdateLocation(_))),
            "no further UpdateLocation expected"
        );
    }
}

async fn wait_for_waiters(lock: &Arc<tokio::sync::Mutex<()>>, count: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while Arc::strong_count(lock) != count {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the lock has {} holders, expected {count}",
            Arc::strong_count(lock)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn agent_options_replace_reaches_a_running_agent() {
    let az = AzuriteServer::start().await;
    az.create_container("rot").await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let sas1 = sas_with(&az, "rot", "rcl", 0);
    let sas2 = sas_with(&az, "rot", "rcl", 30);
    let (api, id) = azure_repo(&ts, &az, "rot", &sas1, false).await;
    let paths = enroll_agent(&ts, &api, &id, "azure-agent").await;
    let agent = agent_id(&paths);
    let stored = |paths: &Paths| -> Value {
        serde_json::from_slice(&std::fs::read(paths.repository_json()).unwrap()).unwrap()
    };
    let before = stored(&paths);
    assert_eq!(before["options"]["sas_token"], sas1);

    let shutdown = CancellationToken::new();
    let running = tokio::spawn(lodestore_agent::run(RunArgs {
        paths: paths.clone(),
        net: NetOptions::default(),
        heartbeat: Duration::from_secs(1),
        shutdown: shutdown.clone(),
    }));
    assert!(
        wait_until(WAIT, || async { ts.state().registry.is_online(&agent) }).await,
        "the agent did not come online"
    );
    let first = marks(&ts, &id).await;

    let (status, replaced) = put_options(&api, &id, json!({ "sas_token": sas2 })).await;
    assert_eq!(status, 200, "{replaced}");
    assert_eq!(replaced["online_notified"], 1, "{replaced}");
    assert_eq!(replaced["needs_update"], 0, "{replaced}");
    let after = marks(&ts, &id).await;
    assert_eq!(generation(&after), generation(&first) + 1);
    let found: Vec<String> =
        serde_json::from_str(&after.get::<String, _>("guarantee_notes_json")).unwrap();
    assert!(
        found.contains(&"lifecycle_unverified".to_string()),
        "{found:?}"
    );
    assert!(
        !found.contains(&"shared_credentials".to_string()),
        "{found:?}"
    );
    assert_eq!(after.get::<i64, _>("guarantee_unchecked_runs"), 0);
    assert_eq!(after.get::<i64, _>("guarantee_failed_runs"), 0);

    assert!(
        wait_until(Duration::from_secs(10), || async {
            stored(&paths)["options"]["sas_token"] == sas2
        })
        .await,
        "the agent did not store the new credentials"
    );
    let mut now = stored(&paths);
    assert!(now["options"].get("account_key").is_none());
    now["options"]["sas_token"] = before["options"]["sas_token"].clone();
    assert_eq!(now, before);
    assert!(ts.state().registry.is_online(&agent));

    assert_eq!(agent_sas(&ts, &id).await, sas2);
    let location =
        serde_json::to_string(&crate::agent_material::stored_location(&ts, &id).await).unwrap();
    assert!(!location.contains(sas_signature(&sas1)));
    assert!(!location.contains(sas_signature(&sas2)));
    shutdown.cancel();
    let _ = running.await;
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn agent_options_replace_counts_agents_that_need_an_update() {
    let az = AzuriteServer::start().await;
    az.create_container("count").await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let sas1 = sas_with(&az, "count", "rcl", 0);
    let sas2 = sas_with(&az, "count", "rcl", 30);
    let (api, id) = azure_repo(&ts, &az, "count", &sas1, false).await;
    let new = enroll_agent(&ts, &api, &id, "new-agent").await;
    let old = enroll_agent(&ts, &api, &id, "old-agent").await;
    let _never = enroll_agent(&ts, &api, &id, "offline-agent").await;

    let mut with = open_session(&ts, &new, &["host_key_v1", "location_v1"]).await;
    let mut without = open_session(&ts, &old, &["host_key_v1"]).await;
    let at_start = next_location(&mut with).await;
    assert_eq!(at_start.backend, "azblob");
    assert_eq!(at_start.repository_id, id);
    assert_eq!(at_start.options["sas_token"], sas1);
    assert!(!at_start.options.contains_key("account_key"));
    silent_of_locations(&mut without).await;

    let (status, replaced) = put_options(&api, &id, json!({ "sas_token": sas2 })).await;
    assert_eq!(status, 200, "{replaced}");
    assert_eq!(replaced["online_notified"], 1, "{replaced}");
    assert_eq!(replaced["needs_update"], 1, "{replaced}");
    assert_eq!(replaced["offline"], 1, "{replaced}");
    let pushed = next_location(&mut with).await;
    let material = writer_material(ts.state(), &id).await.unwrap();
    assert_eq!(pushed.options["sas_token"], sas2);
    assert_eq!(
        pushed
            .options
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>(),
        material.location.options
    );
    silent_of_locations(&mut without).await;
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn agent_options_replace_refuses_without_step_up_or_a_working_probe() {
    let az = AzuriteServer::start().await;
    az.create_container("refuse").await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let sas1 = sas_with(&az, "refuse", "rcl", 0);
    let read_only = sas_with(&az, "refuse", "r", 5);
    let good = sas_with(&az, "refuse", "rcl", 30);
    let (api, id) = azure_repo(&ts, &az, "refuse", &sas1, false).await;
    let paths = enroll_agent(&ts, &api, &id, "azure-agent").await;
    let mut stream = open_session(&ts, &paths, &["host_key_v1", "location_v1"]).await;
    let first = next_location(&mut stream).await;
    assert_eq!(first.options["sas_token"], sas1);
    let before = marks(&ts, &id).await;
    let unchanged = |after: sqlx::any::AnyRow| {
        for column in ["guarantee_level", "guarantee_notes_json", "agent_key_state"] {
            assert_eq!(
                after.get::<String, _>(column),
                before.get::<String, _>(column),
                "{column}"
            );
        }
        assert_eq!(generation(&after), generation(&before));
        assert_eq!(
            after.get::<Option<String>, _>("agent_creds_secret_id"),
            before.get::<Option<String>, _>("agent_creds_secret_id")
        );
    };

    let client = reqwest::Client::new();
    let url = format!("{}{}", ts.rest_url(), options_path(&id));
    let payload = json!({ "agent_options": { "sas_token": good } });
    let anonymous = client.put(&url).json(&payload).send().await.unwrap();
    assert_eq!(anonymous.status(), 401);
    let without_csrf = client
        .put(&url)
        .header("cookie", api.cookie())
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(without_csrf.status(), 403);

    sqlx::query("UPDATE sessions SET authenticated_at = $1")
        .bind(lodestore_server::db::now_ms() - 16 * 60_000)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
    let (status, stale) = put_options(&api, &id, json!({ "sas_token": good })).await;
    assert_eq!(status, 403);
    assert_eq!(stale["code"], "step_up_required");
    unchanged(marks(&ts, &id).await);

    let stepped = api
        .post(
            "/api/v1/auth/step-up",
            &json!({ "password": crate::common::ADMIN_PASSWORD }),
        )
        .await;
    assert_eq!(stepped.status(), 204);

    for (options, expected) in [
        (json!({}), 422),
        (json!({ "account_key": az.account_key() }), 422),
        (json!({ "endpoint": "http://127.0.0.1:1" }), 422),
        (json!({ "sas_token": "" }), 422),
        (json!({ "sas_token": read_only }), 422),
    ] {
        let (status, refused) = put_options(&api, &id, options.clone()).await;
        assert_eq!(status, expected, "{options}: {refused}");
        let text = refused.to_string();
        assert!(!text.contains(sas_signature(&read_only)), "{text}");
        if options.get("sas_token") == Some(&json!(read_only)) {
            assert!(text.contains("could not be verified"), "{text}");
        }
        unchanged(marks(&ts, &id).await);
    }
    let local = ts.create_local_repository("local-repo").await;
    let (status, refused) = put_options(&api, &local, json!({ "sas_token": good })).await;
    assert_eq!(status, 409, "{refused}");
    let (status, _) = put_options(&api, "no-such-repository", json!({ "sas_token": good })).await;
    assert_eq!(status, 404);
    unchanged(marks(&ts, &id).await);
    silent_of_locations(&mut stream).await;
    assert_eq!(agent_sas(&ts, &id).await, sas1);
    ts.stop().await;
}

fn behind_proxy(az: &AzuriteServer, proxy: &FaultProxy) -> String {
    format!("{}/{}", proxy.endpoint(), az.account_name())
}

async fn point_at(ts: &TestServer, id: &str, endpoint: &str) {
    let mut options = crate::agent_material::stored_location(ts, id).await;
    options["endpoint"] = json!(endpoint);
    sqlx::query("UPDATE repositories SET location_json = $1 WHERE id = $2")
        .bind(options.to_string())
        .bind(id)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn agent_options_replace_discards_an_older_check() {
    let az = AzuriteServer::start().await;
    az.create_container("stale").await;
    let ts = TestServer::start_with(&Backend::Sqlite, |c| c.schedule_tick_secs = 0).await;
    let wide = sas_with(&az, "stale", "racwdl", 0);
    let strict = sas_with(&az, "stale", "rcl", 30);
    let strict_again = sas_with(&az, "stale", "rcl", 45);
    let (api, id) = azure_repo(&ts, &az, "stale", &wide, true).await;
    assert_eq!(
        marks(&ts, &id).await.get::<String, _>("guarantee_level"),
        GuaranteeLevel::HostCanOverwriteOrDelete.as_str()
    );
    let upstream = az.endpoint();
    let upstream = upstream
        .strip_suffix(&format!("/{}", az.account_name()))
        .unwrap()
        .to_string();
    let proxy = FaultProxy::start(&upstream, FaultPlan::none()).await;
    point_at(&ts, &id, &behind_proxy(&az, &proxy)).await;

    let at = lodestore_server::db::now_ms() + 25 * crate::agent_material::HOUR;
    let (arrived, release) = proxy.hold("PUT", &[]);
    let never = CancellationToken::new();
    let tick = lodestore_server::maintenance::guarantee::tick(ts.state(), at, &never);
    let (ticked, ()) = tokio::join!(tick, async {
        arrived.await.unwrap();
        let (status, replaced) = put_options(&api, &id, json!({ "sas_token": strict })).await;
        assert_eq!(status, 200, "{replaced}");
        release.send(()).unwrap();
    });
    assert_eq!(ticked.unwrap(), [id.as_str()]);
    let after = marks(&ts, &id).await;
    assert_eq!(generation(&after), 1);
    assert_eq!(
        after.get::<String, _>("guarantee_level"),
        GuaranteeLevel::CreateOnlyEnforced.as_str()
    );
    assert_eq!(agent_sas(&ts, &id).await, strict);

    let (arrived, release) = proxy.hold("PUT", &[]);
    let path = format!("/api/v1/repositories/{id}/check");
    let empty = json!({});
    let (rechecked, ()) = tokio::join!(api.post(&path, &empty), async {
        arrived.await.unwrap();
        let (status, replaced) = put_options(&api, &id, json!({ "sas_token": strict_again })).await;
        assert_eq!(status, 200, "{replaced}");
        release.send(()).unwrap();
    });
    assert_eq!(rechecked.status(), 409);
    let after = marks(&ts, &id).await;
    assert_eq!(generation(&after), 2);
    assert_eq!(agent_sas(&ts, &id).await, strict_again);
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn agent_options_replace_and_session_start_do_not_interleave() {
    let az = AzuriteServer::start().await;
    az.create_container("race").await;
    let ts = TestServer::start_with(&Backend::Sqlite, |c| c.schedule_tick_secs = 0).await;
    let sas1 = sas_with(&az, "race", "rcl", 0);
    let sas3 = sas_with(&az, "race", "rcl", 30);
    let sas4 = sas_with(&az, "race", "rcl", 45);
    let (api, id) = azure_repo(&ts, &az, "race", &sas1, false).await;
    let paths = enroll_agent(&ts, &api, &id, "azure-agent").await;
    let lock = ts.state().repos.access_lock(&id);
    let capabilities = || capabilities_vec(&["host_key_v1", "location_v1"]);

    let held = lock.lock().await;
    wait_for_waiters(&lock, 2).await;
    let put = put_sas(&api, &id, &sas3);
    wait_for_waiters(&lock, 3).await;
    assert_eq!(agent_sas(&ts, &id).await, sas1);
    let opening = tokio::spawn(open_session_at(
        ts.grpc_address(),
        paths.clone(),
        capabilities(),
    ));
    wait_for_waiters(&lock, 4).await;
    assert!(!put.is_finished());
    drop(held);
    let (status, replaced) = put.await.unwrap();
    assert_eq!(status, 200, "{replaced}");
    assert_eq!(replaced["online_notified"], 1, "{replaced}");
    let mut stream = opening.await.unwrap();
    assert_eq!(next_location(&mut stream).await.options["sas_token"], sas3);
    assert_eq!(next_location(&mut stream).await.options["sas_token"], sas3);
    silent_of_locations(&mut stream).await;
    drop(stream);

    let held = lock.lock().await;
    let opening = tokio::spawn(open_session_at(
        ts.grpc_address(),
        paths.clone(),
        capabilities(),
    ));
    wait_for_waiters(&lock, 3).await;
    let put = put_sas(&api, &id, &sas4);
    wait_for_waiters(&lock, 4).await;
    drop(held);
    let mut stream = opening.await.unwrap();
    let (status, replaced) = put.await.unwrap();
    assert_eq!(status, 200, "{replaced}");
    assert_eq!(replaced["online_notified"], 1, "{replaced}");
    assert_eq!(next_location(&mut stream).await.options["sas_token"], sas3);
    assert_eq!(next_location(&mut stream).await.options["sas_token"], sas4);
    silent_of_locations(&mut stream).await;
    assert_eq!(agent_sas(&ts, &id).await, sas4);
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn agent_options_replacement_is_never_echoed() {
    let logs = capture_logs();
    let az = AzuriteServer::start().await;
    az.create_container("quiet").await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let sas1 = sas_with(&az, "quiet", "rcl", 0);
    let sas2 = sas_with(&az, "quiet", "rcl", 30);
    let sas3 = sas_with(&az, "quiet", "rcl", 45);
    let read_only = sas_with(&az, "quiet", "r", 5);
    let signatures = [&sas1, &sas2, &sas3, &read_only].map(|s| sas_signature(s).to_string());
    let (api, id) = azure_repo(&ts, &az, "quiet", &sas1, false).await;
    let mut seen = String::new();

    for sas in [&sas2, &read_only, &sas3] {
        let response = api
            .put(
                &options_path(&id),
                &json!({ "agent_options": { "sas_token": sas } }),
            )
            .await;
        let expected = if *sas == read_only { 422 } else { 200 };
        assert_eq!(response.status(), expected);
        seen.push_str(&response.text().await.unwrap());
    }
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
    assert_eq!(
        secrets
            .iter()
            .filter(|r| r.get::<String, _>("kind") == "repo-agent-creds")
            .count(),
        1,
        "the replaced agent secret is gone"
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
    assert_eq!(agent_sas(&ts, &id).await, sas3);

    let log = logs.take();
    for signature in &signatures {
        assert!(
            !seen.contains(signature.as_str()),
            "a signature reached a response or a column"
        );
        assert!(
            !log.contains(signature.as_str()),
            "a signature reached the server log"
        );
    }
    ts.stop().await;
}
