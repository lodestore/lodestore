//! The level a probe reports against what an agent's own credentials can do.
//! Each configuration is probed, then attacked with raw requests that bypass
//! the store (an unconditional overwrite, a delete and, on SFTP, a rewrite
//! through a handle held open); the level must not promise more than the
//! attack left standing. The same check for MinIO lives in
//! `it::adv_storage_b2`.

use std::time::Duration;

use bytes::Bytes;
use lodestore_format::Error;
use lodestore_format::store::probe::{AgentProbe, GuaranteeLevel, probe};
use lodestore_format::store::sftp::Tear;
use lodestore_format::store::{BlobStore, Store, put_named};
use lodestore_testkit::azurite::AzuriteServer;
use lodestore_testkit::sshd::{SshUser, SshdServer};
use russh_sftp::protocol::OpenFlags;
use tokio::io::AsyncWriteExt;

use super::adv_storage_sftp::attacker;
use super::azblob::{account_store, sas_store};
use super::gcs::store as gcs_store;
use super::sftp::{direct_agent, store};
use lodestore_testkit::gcs::FakeGcsServer;

const ORIGINAL: &[u8] = b"original pack bytes";
const FORGED: &[u8] = b"forged pack bytes";

#[derive(Debug)]
struct Outcome {
    overwrote: bool,
    deleted: bool,
    original_current: bool,
}

/// `create_only_enforced` must survive every attack; `host_can_overwrite_or_delete`
/// must be earned by at least one attack or by a store that can publish a
/// torn object. The levels that rest on earlier versions are checked on MinIO.
fn assert_level_matches(what: &str, level: GuaranteeLevel, atomic: bool, o: &Outcome) {
    match level {
        GuaranteeLevel::CreateOnlyEnforced => assert!(
            !o.overwrote && !o.deleted && o.original_current,
            "{what}: create_only_enforced but {o:?}"
        ),
        GuaranteeLevel::HostCanOverwriteOrDelete => assert!(
            o.overwrote || o.deleted || !atomic,
            "{what}: host_can_overwrite_or_delete but no attack worked: {o:?}"
        ),
        other => panic!("{what}: {other:?} is not reachable on this backend"),
    }
}

fn azure_operator(az: &AzuriteServer, container: &str, sas: Option<&str>) -> opendal::Operator {
    opendal::install_default();
    let builder = opendal::services::Azblob::default()
        .endpoint(&az.endpoint())
        .container(container);
    let builder = match sas {
        Some(sas) => builder.sas_token(sas),
        None => builder
            .account_name(az.account_name())
            .account_key(az.account_key()),
    };
    opendal::Operator::new(builder).unwrap()
}

async fn attack_azure(op: &opendal::Operator, server: &Store, key: &str) -> Outcome {
    server
        .put_if_absent(key, Bytes::from_static(ORIGINAL))
        .await
        .unwrap();
    let overwrote = op.write(key, FORGED.to_vec()).await.is_ok();
    let original_current = server.get(key).await.is_ok_and(|b| b[..] == *ORIGINAL);
    let deleted = op.delete(key).await.is_ok();
    Outcome {
        overwrote,
        deleted,
        original_current,
    }
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn adv_service_level_matches_attacks() {
    let az = AzuriteServer::start().await;
    for container in ["accountkey", "sas"] {
        az.create_container(container).await;
    }
    let sas = az.container_sas("sas", "rcl", Duration::from_secs(3600));
    let azure = [
        (
            "azure, account key as agent",
            account_store(&az, "accountkey").await,
            azure_operator(&az, "accountkey", None),
            "accountkey",
            GuaranteeLevel::HostCanOverwriteOrDelete,
        ),
        (
            "azure, rcl SAS",
            sas_store(&az, "sas", "rcl"),
            azure_operator(&az, "sas", Some(&sas)),
            "sas",
            GuaranteeLevel::CreateOnlyEnforced,
        ),
    ];
    for (what, agent, op, container, expected) in azure {
        let server = account_store(&az, container).await;
        let report = probe(&server, AgentProbe::Store(&agent)).await;
        assert_eq!(report.guarantee, expected, "{what}: {report:?}");
        let outcome = attack_azure(&op, &server, "packs/victim").await;
        assert_level_matches(
            what,
            report.guarantee,
            agent.publishes_atomically(),
            &outcome,
        );
    }

    let sshd = SshdServer::start().await;
    let srv = store(&sshd, SshUser::Srv);
    srv.ensure_prefixes(&["packs", "index", "snapshots"])
        .await
        .unwrap();
    let sftp = [
        ("sftp, agent", SshUser::Agent, false),
        ("sftp, wide user", SshUser::Wide, false),
        ("sftp, server's own user", SshUser::Srv, false),
        ("sftp, direct publish", SshUser::NoLink, true),
    ];
    for (n, (what, user, direct)) in sftp.into_iter().enumerate() {
        let agent = if direct {
            direct_agent(&sshd, sshd.root(), [])
        } else {
            store(&sshd, user)
        };
        let report = probe(&srv, AgentProbe::Store(&agent)).await;
        assert_eq!(
            report.guarantee,
            GuaranteeLevel::HostCanOverwriteOrDelete,
            "{what}: {report:?}"
        );
        let outcome = attack_sftp(&sshd, &srv, user, direct, n).await;
        assert_level_matches(
            what,
            report.guarantee,
            agent.publishes_atomically(),
            &outcome,
        );
    }
}

async fn attack_sftp(
    sshd: &SshdServer,
    server: &Store,
    user: SshUser,
    direct: bool,
    n: usize,
) -> Outcome {
    let key = format!("packs/0{n}/victim");
    let held = format!("packs/0{n}/held");
    server
        .put_if_absent(&key, Bytes::from_static(ORIGINAL))
        .await
        .unwrap();
    let raw = attacker(sshd, user).await;
    let s = &raw.sftp;
    let write = OpenFlags::WRITE;

    let mut overwrote = false;
    if let Ok(mut file) = s
        .open_with_flags(format!("/data/{key}"), write | OpenFlags::TRUNCATE)
        .await
    {
        overwrote = file.write_all(FORGED).await.is_ok();
        let _ = file.shutdown().await;
    }
    let original_current = server.get(&key).await.is_ok_and(|b| b[..] == *ORIGINAL);
    let deleted = s.remove_file(format!("/data/{key}")).await.is_ok();

    // Publish a file and keep writing to it through the handle that made it.
    let create = write | OpenFlags::CREATE | OpenFlags::EXCLUDE;
    let final_path = format!("/data/{held}");
    let mut handle = if direct {
        s.open_with_flags(&final_path, create).await.unwrap()
    } else {
        let tmp = format!("/data/tmp/held-{n}");
        let file = s.open_with_flags(&tmp, create).await.unwrap();
        s.hardlink(&tmp, &final_path).await.unwrap();
        file
    };
    handle.write_all(b"honest").await.unwrap();
    handle.flush().await.unwrap();
    handle.write_all(b" and rewritten").await.unwrap();
    handle.shutdown().await.unwrap();
    let rewritten = server
        .get(&held)
        .await
        .is_ok_and(|b| b[..] == *b"honest and rewritten");

    Outcome {
        overwrote: overwrote || rewritten,
        deleted,
        original_current,
    }
}

async fn assert_short_object_is_not_counted(planter: &Store, agent: &Store, key: &str) {
    planter
        .put_if_absent(key, Bytes::from(vec![1u8; 100]))
        .await
        .unwrap();
    let err = put_named(agent, key, vec![1u8; 4096]).await.unwrap_err();
    assert!(matches!(err, Error::NameBurned { .. }), "{key}: {err:?}");
}

/// A full-size object that differs from the one written. A store that
/// publishes atomically cannot produce one by itself, so a name holding
/// full-size bytes is accepted by size; only a store that can leave a torn
/// object is compared byte for byte.
async fn assert_full_size_with_hole(planter: &Store, agent: &Store, key: &str) {
    let mut holed = vec![1u8; 4096];
    holed[..2048].fill(0);
    planter
        .put_if_absent(key, Bytes::from(holed))
        .await
        .unwrap();
    let result = put_named(agent, key, vec![1u8; 4096]).await;
    if agent.publishes_atomically() {
        result.unwrap();
    } else {
        assert!(
            matches!(result, Err(Error::NameBurned { .. })),
            "{key}: {result:?}"
        );
    }
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn adv_client_short_object_is_not_counted() {
    let az = AzuriteServer::start().await;
    az.create_container("short").await;
    let owner = account_store(&az, "short").await;
    let agent = sas_store(&az, "short", "rcl");
    assert_short_object_is_not_counted(&owner, &agent, "packs/short").await;
    assert_full_size_with_hole(&owner, &agent, "packs/holed").await;

    let gcs = FakeGcsServer::start().await;
    gcs.create_bucket("short").await;
    let bucket = gcs_store(&gcs, "short");
    assert_short_object_is_not_counted(&bucket, &bucket, "packs/short").await;
    assert_full_size_with_hole(&bucket, &bucket, "packs/holed").await;

    let sshd = SshdServer::start().await;
    let srv = store(&sshd, SshUser::Srv);
    srv.ensure_prefixes(&["packs"]).await.unwrap();
    let linking = store(&sshd, SshUser::Agent);
    assert_short_object_is_not_counted(&srv, &linking, "packs/00/short").await;
    assert_full_size_with_hole(&srv, &linking, "packs/00/holed").await;
    let direct = direct_agent(&sshd, sshd.root(), [None::<Tear>]);
    assert_short_object_is_not_counted(&srv, &direct, "packs/01/short").await;
    assert_full_size_with_hole(&srv, &direct, "packs/01/holed").await;
}
