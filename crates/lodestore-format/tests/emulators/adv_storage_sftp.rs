//! A compromised agent against an SFTP repository, on a real OpenSSH server.
//! `adv_client_*` read the sftp-server request log to show what the client
//! sends; `adv_service_*` attack with the agent's own credentials to show what
//! the server refuses.

use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use lodestore_format::store::probe::{AgentProbe, CheckId, GuaranteeLevel, probe};
use lodestore_format::store::sftp::Tear;
use lodestore_format::store::{BlobStore, Store};
use lodestore_testkit::sshd::{SshUser, SshdServer};
use russh::client::{self, Handle};
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh_sftp::client::SftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
use tokio::io::AsyncWriteExt;

use super::sftp::{
    assert_no_destructive_requests, direct_agent, messages, modes, sftp_of, store, tail_after,
    weaknesses,
};

struct AnyHost;

impl client::Handler for AnyHost {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

/// An SFTP session opened by hand with a test user's key, so a request the
/// store would never send can be sent.
struct Attacker {
    sftp: SftpSession,
    _handle: Handle<AnyHost>,
}

async fn attacker(sshd: &SshdServer, user: SshUser) -> Attacker {
    let key = PrivateKey::from_openssh(sshd.private_key(user).trim()).unwrap();
    let mut handle = client::connect(Arc::default(), sshd.endpoint(), AnyHost)
        .await
        .unwrap();
    let auth = handle
        .authenticate_publickey(
            user.login(),
            PrivateKeyWithHashAlg::new(Arc::new(key), None),
        )
        .await
        .unwrap();
    assert!(auth.success(), "{user:?} was refused");
    let channel = handle.channel_open_session().await.unwrap();
    channel.request_subsystem(true, "sftp").await.unwrap();
    let sftp = SftpSession::new(channel.into_stream()).await.unwrap();
    Attacker {
        sftp,
        _handle: handle,
    }
}

fn assert_denied<T>(what: &str, result: Result<T, SftpError>) {
    match result {
        Err(SftpError::Status(s)) if s.status_code == StatusCode::PermissionDenied => {}
        Err(e) => panic!("{what}: expected permission denied, got {e}"),
        Ok(_) => panic!("{what}: the server allowed it"),
    }
}

const WRITE: OpenFlags = OpenFlags::WRITE;

async fn server_with_prefixes(sshd: &SshdServer) -> Store {
    let server = store(sshd, SshUser::Srv);
    server
        .ensure_prefixes(&["packs", "index", "snapshots"])
        .await
        .unwrap();
    server
}

fn make_small_tree() -> tempfile::TempDir {
    let src = tempfile::tempdir().unwrap();
    super::make_tree(src.path(), 2);
    src
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn adv_client_sftp_backup_sends_only_exclusive_creates() {
    let sshd = SshdServer::start().await;
    let server = store(&sshd, SshUser::Srv);
    super::init_repo(&server).await;

    let agent = sftp_of(store(&sshd, SshUser::Agent));
    let container = sshd.container().to_owned();
    let fired = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&fired);
    agent.on_before_hardlink(move |_tmp| {
        if !seen.swap(true, Ordering::SeqCst) {
            let status = Command::new("docker")
                .args(["exec", &container, "pkill", "-KILL", "-u", "agent"])
                .status()
                .unwrap();
            assert!(status.success());
        }
    });
    let agent = Store::Sftp(agent);

    let mark = sshd.log().len();
    let src = make_small_tree();
    let first = super::backup(&agent, src.path()).await;
    let second = super::backup(&agent, src.path()).await;
    let log = sshd.log();
    let log = tail_after(&log, mark);
    super::restore_and_compare(&server, &first, src.path()).await;
    super::restore_and_compare(&server, &second, src.path()).await;
    assert!(fired.load(Ordering::SeqCst), "the connection was never cut");

    let ops = messages(log);
    let writes: Vec<&&str> = ops
        .iter()
        .filter(|m| m.starts_with("open ") && m.contains("WRITE"))
        .collect();
    assert!(writes.len() >= 3, "{log}");
    for line in writes {
        assert!(
            line.contains("CREATE") && line.contains("EXCL") && line.contains("\"/data/tmp/"),
            "{line}"
        );
        assert!(!line.contains("TRUNC"), "{line}");
    }
    assert!(ops.iter().any(|m| m.starts_with("hardlink ")), "{log}");
    assert_no_destructive_requests(&ops);
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn adv_client_sftp_direct_mode_never_truncates() {
    let sshd = SshdServer::start().await;
    let server = store(&sshd, SshUser::Srv);
    super::init_repo(&server).await;
    let agent = direct_agent(&sshd, sshd.root(), [Some(Tear::Short(4))]);

    let mark = sshd.log().len();
    let src = make_small_tree();
    let outcome = super::backup(&agent, src.path()).await;
    let log = sshd.log();
    let log = tail_after(&log, mark);
    super::restore_and_compare(&server, &outcome, src.path()).await;

    let ops = messages(log);
    let writes: Vec<&&str> = ops
        .iter()
        .filter(|m| m.starts_with("open ") && m.contains("WRITE"))
        .collect();
    assert!(writes.len() >= 3, "{log}");
    for line in writes {
        assert!(line.contains("CREATE") && line.contains("EXCL"), "{line}");
        assert!(
            !line.contains("TRUNC") && !line.contains("/data/tmp/"),
            "{line}"
        );
    }
    assert!(!ops.iter().any(|m| m.starts_with("hardlink")), "{ops:?}");
    assert_no_destructive_requests(&ops);
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn adv_service_sftp_agent_attacks_are_refused() {
    let sshd = SshdServer::start().await;
    let server = server_with_prefixes(&sshd).await;
    let agent = store(&sshd, SshUser::Agent);
    agent
        .put_if_absent("packs/03/victim", Bytes::from_static(b"original"))
        .await
        .unwrap();
    let victim = "/data/packs/03/victim";

    let raw = attacker(&sshd, SshUser::Agent).await;
    let s = &raw.sftp;
    assert_denied(
        "reopen with truncate",
        s.open_with_flags(victim, WRITE | OpenFlags::TRUNCATE).await,
    );
    assert_denied("reopen for write", s.open_with_flags(victim, WRITE).await);
    assert_denied(
        "reopen for append",
        s.open_with_flags(victim, WRITE | OpenFlags::APPEND).await,
    );
    assert_denied("remove", s.remove_file(victim).await);

    let tmp = "/data/tmp/attack";
    let mut staged = s
        .open_with_flags(tmp, WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE)
        .await
        .unwrap();
    staged.write_all(b"forged").await.unwrap();
    staged.shutdown().await.unwrap();
    assert_denied("rename over the pack", s.rename(tmp, victim).await);
    assert!(
        s.hardlink(tmp, victim).await.is_err(),
        "a hardlink replaced an existing pack"
    );
    let chmod = FileAttributes {
        permissions: Some(0o666),
        ..FileAttributes::empty()
    };
    assert_denied("chmod", s.set_metadata(victim, chmod).await);
    assert_denied("mkdir", s.create_dir("/data/packs/zz").await);
    assert_denied("symlink", s.symlink("/data/packs/03/link", victim).await);

    assert_eq!(
        &server.get("packs/03/victim").await.unwrap()[..],
        b"original"
    );
    let published = modes(&sshd)
        .into_iter()
        .find(|(_, path)| path.ends_with("/victim"))
        .unwrap();
    assert_eq!(published.0, "444");
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn adv_service_sftp_agent_cannot_truncate_server_files() {
    let sshd = SshdServer::start().await;
    let server = store(&sshd, SshUser::Srv);
    super::init_repo(&server).await;
    let slots = server.list("keys/").await.unwrap();
    assert!(!slots.is_empty());
    let mut targets = vec!["config".to_owned()];
    targets.extend(slots.into_iter().map(|e| e.key));

    let raw = attacker(&sshd, SshUser::Agent).await;
    for key in &targets {
        let before = server.get(key).await.unwrap();
        let path = format!("/data/{key}");
        assert_denied(
            "truncate",
            raw.sftp
                .open_with_flags(path.as_str(), WRITE | OpenFlags::TRUNCATE)
                .await,
        );
        assert_denied(
            "write",
            raw.sftp.open_with_flags(path.as_str(), WRITE).await,
        );
        assert_eq!(server.get(key).await.unwrap(), before, "{key}");
    }
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn adv_service_sftp_held_handle_is_not_blocked() {
    let sshd = SshdServer::start().await;
    let server = server_with_prefixes(&sshd).await;
    let raw = attacker(&sshd, SshUser::Agent).await;
    let s = &raw.sftp;

    let tmp = "/data/tmp/held";
    let mut held = s
        .open_with_flags(tmp, WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE)
        .await
        .unwrap();
    held.write_all(b"honest").await.unwrap();
    held.flush().await.unwrap();
    assert!(s.hardlink(tmp, "/data/packs/04/held").await.unwrap());
    assert_eq!(&server.get("packs/04/held").await.unwrap()[..], b"honest");

    held.write_all(b" and then rewritten").await.unwrap();
    held.shutdown().await.unwrap();
    assert_eq!(
        &server.get("packs/04/held").await.unwrap()[..],
        b"honest and then rewritten",
        "the published file did not change: the limit of this protection moved"
    );

    let agent = store(&sshd, SshUser::Agent);
    let report = probe(&server, AgentProbe::Store(&agent)).await;
    assert_eq!(report.guarantee, GuaranteeLevel::HostCanOverwriteOrDelete);
    assert_eq!(weaknesses(&report), ["held_handle_rewrite"], "{report:?}");
    let detail = report.check(CheckId::AgentDelete).detail.clone().unwrap();
    assert!(detail.contains("held handle"), "{detail}");
}
