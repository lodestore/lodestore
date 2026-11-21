use crate::agent_material::{body, create, sftp_draft};
use crate::common::{Backend, TestServer};
use lodestore_format::store::probe::GuaranteeLevel;
use lodestore_format::store::{BlobStore, RepoLocation};
use lodestore_server::repos::db::{server_access, writer_material};
use lodestore_testkit::sshd::{SshUser, SshdServer};
use serde_json::{Value, json};

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
