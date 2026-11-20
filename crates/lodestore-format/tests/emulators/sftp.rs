use std::process::Command;

use bytes::Bytes;
use lodestore_format::store::{BlobDelete, BlobError, BlobPurge, BlobStore, RepoLocation, Store};
use lodestore_testkit::sshd::{SshUser, SshdServer};

const REPO: &str = "/srv/repo/data";

fn location(sshd: &SshdServer, user: SshUser, host_key: &str, extra: &[(&str, &str)]) -> Store {
    let key = sshd.private_key(user);
    let endpoint = sshd.endpoint();
    let mut options = vec![
        ("endpoint", endpoint.as_str()),
        ("user", user.login()),
        ("key", key.as_str()),
        ("host_key", host_key),
        ("root", sshd.root()),
    ];
    options.extend_from_slice(extra);
    RepoLocation::new("sftp", options).open(None).unwrap()
}

fn store(sshd: &SshdServer, user: SshUser) -> Store {
    location(sshd, user, &sshd.host_key(), &[])
}

fn exec(sshd: &SshdServer, args: &[&str]) -> String {
    let out = Command::new("docker")
        .arg("exec")
        .arg(sshd.container())
        .args(args)
        .output()
        .expect("docker exec");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn modes(sshd: &SshdServer) -> Vec<(String, String)> {
    exec(sshd, &["find", REPO, "-type", "f", "-printf", "%m %p\\n"])
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(m, p)| (m.to_owned(), p.to_owned()))
        .collect()
}

/// The sftp-server messages of a log slice. The syslog socket delivers them
/// back to back, each led by its priority tag.
fn messages(log: &str) -> Vec<&str> {
    log.split("<38>")
        .skip(1)
        .filter_map(|m| m.split_once("]: ").map(|(_, rest)| rest.trim()))
        .collect()
}

/// No request that could replace, hide or remove an object, and no refusal
/// beyond the extension advertisement every session starts with.
fn assert_no_destructive_requests(ops: &[&str]) {
    const STARTUP_REFUSALS: [&str; 4] = ["posix-rename", "lsetstat", "copy-data", "hardlink"];
    for op in ops {
        if let Some(rest) = op.strip_prefix("Refusing denylisted ") {
            assert!(
                STARTUP_REFUSALS.iter().any(|r| rest.starts_with(r)),
                "the agent asked for something it may not do: {op}"
            );
            continue;
        }
        let verb = op.split(' ').next().unwrap_or("");
        for forbidden in [
            "remove", "rmdir", "rename", "setstat", "fsetstat", "mkdir", "symlink",
        ] {
            assert_ne!(verb, forbidden, "{op}");
        }
        assert!(!op.contains("TRUNC"), "{op}");
    }
}

fn tail_after(log: &str, mark: usize) -> &str {
    log.get(mark..).unwrap_or("")
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sftp_init_backup_restore_through_hardlink() {
    let sshd = SshdServer::start().await;
    let server = store(&sshd, SshUser::Srv);
    let agent = store(&sshd, SshUser::Agent);

    super::init_repo(&server).await;
    let mark = sshd.log().len();

    let src = tempfile::tempdir().unwrap();
    super::make_tree(src.path(), 3);
    let outcome = super::backup(&agent, src.path()).await;
    let agent_log = sshd.log();
    let agent_log = tail_after(&agent_log, mark);

    super::restore_and_compare(&server, &outcome, src.path()).await;

    let ops = messages(agent_log);
    let opens: Vec<&str> = ops
        .iter()
        .copied()
        .filter(|m| m.starts_with("open "))
        .collect();
    assert!(!opens.is_empty(), "{agent_log}");
    for line in &opens {
        if line.contains("WRITE") {
            assert!(
                line.contains("CREATE") && line.contains("EXCL") && line.contains("\"/data/tmp/"),
                "{line}"
            );
            assert!(!line.contains("TRUNC"), "{line}");
        }
    }
    assert!(
        ops.iter().any(|m| m.starts_with("hardlink ")),
        "{agent_log}"
    );
    assert_no_destructive_requests(&ops);

    let files = modes(&sshd);
    for name in ["/config", "/keys/", "/packs/"] {
        assert!(
            files.iter().any(|(_, p)| p.contains(name)),
            "no file matching {name}: {files:?}"
        );
    }
    for (mode, path) in &files {
        assert_eq!(mode, "444", "{path}");
    }
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sftp_put_twice_is_already_exists() {
    let sshd = SshdServer::start().await;
    let server = store(&sshd, SshUser::Srv);
    super::put_twice_is_already_exists(&server).await;
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sftp_failure_without_object_is_backend_error() {
    let sshd = SshdServer::start().await;
    let server = store(&sshd, SshUser::Srv);
    let agent = store(&sshd, SshUser::Agent);
    server.ensure_prefixes(&["packs"]).await.unwrap();
    let too_long = format!("packs/00/{}", "a".repeat(300));
    let err = agent
        .put_if_absent(&too_long, Bytes::from_static(b"x"))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            BlobError::Backend {
                temporary: false,
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sftp_missing_directory_is_denied_for_the_agent() {
    let sshd = SshdServer::start().await;
    let server = store(&sshd, SshUser::Srv);
    let agent = store(&sshd, SshUser::Agent);
    server.ensure_prefixes(&["packs"]).await.unwrap();
    let err = agent
        .put_if_absent("packs/zz/obj", Bytes::from_static(b"x"))
        .await
        .unwrap_err();
    assert!(matches!(err, BlobError::Denied { .. }), "{err:?}");
    assert_eq!(server.object_size("packs/zz/obj").await.unwrap(), None);
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sftp_vanished_tmp_is_retried_with_a_new_name() {
    let sshd = SshdServer::start().await;
    let server = store(&sshd, SshUser::Srv);
    server.ensure_prefixes(&["packs"]).await.unwrap();
    let Store::Sftp(agent) = store(&sshd, SshUser::Agent) else {
        panic!("not an SFTP store");
    };
    let container = sshd.container().to_owned();
    let fired = std::sync::atomic::AtomicBool::new(false);
    agent.on_before_hardlink(move |tmp| {
        if !fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let real = format!("/srv/repo{tmp}");
            let status = Command::new("docker")
                .args(["exec", &container, "rm", "-f", &real])
                .status()
                .unwrap();
            assert!(status.success());
        }
    });
    let mark = sshd.log().len();
    let agent = Store::Sftp(agent);
    agent
        .put_if_absent("packs/00/vanish", Bytes::from_static(b"payload"))
        .await
        .unwrap();
    assert_eq!(&agent.get("packs/00/vanish").await.unwrap()[..], b"payload");
    let log = sshd.log();
    let opens = messages(tail_after(&log, mark))
        .into_iter()
        .filter(|m| m.starts_with("open \"/data/tmp/") && m.contains("EXCL"))
        .count();
    assert_eq!(opens, 2, "{log}");
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sftp_get_range_past_end_is_short() {
    let sshd = SshdServer::start().await;
    let server = store(&sshd, SshUser::Srv);
    server.ensure_prefixes(&["packs"]).await.unwrap();
    server
        .put_if_absent("packs/01/obj", Bytes::from_static(b"0123456789"))
        .await
        .unwrap();
    assert_eq!(
        &server.get_range("packs/01/obj", 2..5).await.unwrap()[..],
        b"234"
    );
    assert_eq!(
        &server.get_range("packs/01/obj", 8..100).await.unwrap()[..],
        b"89"
    );
    assert!(
        server
            .get_range("packs/01/obj", 10..20)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        server
            .get_range("packs/01/obj", 3..3)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        server.get_range("packs/01/none", 0..1).await,
        Err(BlobError::NotFound)
    ));
    assert!(matches!(
        server.get("packs/01/none").await,
        Err(BlobError::NotFound)
    ));
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sftp_list_returns_nested_pack_keys_with_sizes() {
    let sshd = SshdServer::start().await;
    let server = store(&sshd, SshUser::Srv);
    server.ensure_prefixes(&["packs", "index"]).await.unwrap();
    for (key, body) in [
        ("packs/00/aa", &b"1"[..]),
        ("packs/ff/bb", &b"22"[..]),
        ("index/one", &b"333"[..]),
    ] {
        server
            .put_if_absent(key, Bytes::copy_from_slice(body))
            .await
            .unwrap();
    }
    let packs = server.list("packs/").await.unwrap();
    let shape: Vec<_> = packs.iter().map(|e| (e.key.as_str(), e.size)).collect();
    assert_eq!(shape, vec![("packs/00/aa", 1), ("packs/ff/bb", 2)]);
    assert!(packs.iter().all(|e| e.modified_ms.is_some()));
    let all = server.list("").await.unwrap();
    assert_eq!(all.len(), 3, "{all:?}");
    assert!(all.iter().all(|e| !e.key.starts_with("tmp/")));
    assert_eq!(server.list("packs/f").await.unwrap().len(), 1);
    assert!(server.list("snapshots/").await.unwrap().is_empty());
    assert_eq!(server.object_size("index/one").await.unwrap(), Some(3));
    assert_eq!(server.object_size("index/none").await.unwrap(), None);
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sftp_server_delete_and_purge() {
    let sshd = SshdServer::start().await;
    let server = store(&sshd, SshUser::Srv);
    server.ensure_prefixes(&["packs"]).await.unwrap();
    server
        .put_if_absent("packs/02/gone", Bytes::from_static(b"12345"))
        .await
        .unwrap();
    server.delete("packs/02/gone").await.unwrap();
    assert!(matches!(
        server.delete("packs/02/gone").await,
        Err(BlobError::NotFound)
    ));
    server
        .put_if_absent("packs/02/purged", Bytes::from_static(b"12345"))
        .await
        .unwrap();
    assert!(matches!(
        server.purge("packs/02/purged").await,
        lodestore_format::store::PurgeOutcome::Purged {
            versions: 1,
            bytes: 5
        }
    ));
    assert!(matches!(
        server.purge("packs/02/purged").await,
        lodestore_format::store::PurgeOutcome::NotFound
    ));
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sftp_agent_cannot_delete_rename_or_mkdir() {
    let sshd = SshdServer::start().await;
    let server = store(&sshd, SshUser::Srv);
    let agent = store(&sshd, SshUser::Agent);
    server.ensure_prefixes(&["packs"]).await.unwrap();
    agent
        .put_if_absent("packs/03/mine", Bytes::from_static(b"keep"))
        .await
        .unwrap();
    assert!(matches!(
        agent.delete("packs/03/mine").await,
        Err(BlobError::Denied { .. })
    ));
    let Store::Sftp(_) = &agent else { panic!() };
    let err = agent
        .put_if_absent("packs/zz/new-dir-object", Bytes::from_static(b"x"))
        .await
        .unwrap_err();
    assert!(matches!(err, BlobError::Denied { .. }), "{err:?}");
    assert!(agent.ensure_prefixes(&["brand-new"]).await.is_err());
    assert_eq!(&server.get("packs/03/mine").await.unwrap()[..], b"keep");
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sftp_reconnects_after_the_connection_drops() {
    let sshd = SshdServer::start().await;
    let server = store(&sshd, SshUser::Srv);
    server.ensure_prefixes(&["packs"]).await.unwrap();
    server
        .put_if_absent("packs/04/first", Bytes::from_static(b"one"))
        .await
        .unwrap();
    exec(&sshd, &["pkill", "-KILL", "-u", "srv"]);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    server
        .put_if_absent("packs/04/second", Bytes::from_static(b"two"))
        .await
        .unwrap();
    exec(&sshd, &["pkill", "-KILL", "-u", "srv"]);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(&server.get("packs/04/first").await.unwrap()[..], b"one");
    assert_eq!(&server.get("packs/04/second").await.unwrap()[..], b"two");
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sftp_agent_cannot_overwrite_a_probe_object() {
    use lodestore_format::store::probe::{AgentKeyState, AgentProbe, probe};

    let sshd = SshdServer::start().await;
    let server = store(&sshd, SshUser::Srv);
    server
        .ensure_prefixes(&["packs", "index", "snapshots"])
        .await
        .unwrap();

    let agent = store(&sshd, SshUser::Agent);
    let report = probe(&server, AgentProbe::Store(&agent)).await;
    assert_eq!(report.agent_key, AgentKeyState::Verified, "{report:?}");

    let wide = store(&sshd, SshUser::Wide);
    let report = probe(&server, AgentProbe::Store(&wide)).await;
    assert_ne!(report.agent_key, AgentKeyState::Verified, "{report:?}");
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sftp_direct_mode_publishes_without_hardlink() {
    let sshd = SshdServer::start().await;
    let direct = [("publish", "direct")];
    let server = location(&sshd, SshUser::Srv, &sshd.host_key(), &[]);
    let agent = location(&sshd, SshUser::NoLink, &sshd.host_key(), &direct);
    assert!(!agent.publishes_atomically());
    assert!(server.publishes_atomically());

    super::init_repo(&server).await;
    let mark = sshd.log().len();
    let src = tempfile::tempdir().unwrap();
    super::make_tree(src.path(), 3);
    let outcome = super::backup(&agent, src.path()).await;
    let log = sshd.log();
    let ops = messages(tail_after(&log, mark));
    super::restore_and_compare(&server, &outcome, src.path()).await;

    assert!(!ops.iter().any(|m| m.starts_with("hardlink")), "{ops:?}");
    let writes: Vec<&&str> = ops
        .iter()
        .filter(|m| m.starts_with("open ") && m.contains("WRITE"))
        .collect();
    assert!(!writes.is_empty(), "{ops:?}");
    for line in writes {
        assert!(line.contains("CREATE") && line.contains("EXCL"), "{line}");
        assert!(
            !line.contains("TRUNC") && !line.contains("/data/tmp/"),
            "{line}"
        );
    }
    assert_no_destructive_requests(&ops);
    for (mode, path) in modes(&sshd) {
        assert_eq!(mode, "444", "{path}");
    }
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sftp_hardlink_mode_on_a_server_refusing_hardlink_fails() {
    let sshd = SshdServer::start().await;
    let server = store(&sshd, SshUser::Srv);
    server.ensure_prefixes(&["packs"]).await.unwrap();
    let nolink = store(&sshd, SshUser::NoLink);
    let mark = sshd.log().len();
    let err = nolink
        .put_if_absent("packs/00/obj", Bytes::from_static(b"x"))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, BlobError::Backend { temporary: false, message } if message.contains("publish = direct")),
        "{err:?}"
    );
    assert_eq!(server.object_size("packs/00/obj").await.unwrap(), None);
    let log = sshd.log();
    let ops = messages(tail_after(&log, mark));
    assert!(
        !ops.iter()
            .any(|m| m.starts_with("open ") && m.contains("WRITE")),
        "{ops:?}"
    );
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sftp_hardlink_mode_without_the_extension_fails() {
    let sshd = SshdServer::start().await;
    let Store::Sftp(inner) = store(&sshd, SshUser::Srv) else {
        panic!("not an SFTP store");
    };
    inner.pretend_missing_extension("hardlink@openssh.com");
    let server = Store::Sftp(inner);
    let err = server
        .put_if_absent("keys/obj", Bytes::from_static(b"x"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("publish = direct"), "{err}");
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sftp_changed_host_key_is_a_hard_error() {
    let sshd = SshdServer::start().await;
    let pinned = sshd.host_key();
    let before = location(&sshd, SshUser::Srv, &pinned, &[]);
    before.ensure_prefixes(&["packs"]).await.unwrap();

    sshd.rotate_host_key();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let after = location(&sshd, SshUser::Srv, &pinned, &[]);
    for _ in 0..2 {
        let err = after.get("config").await.unwrap_err();
        let BlobError::HostKeyMismatch { pinned, offered } = &err else {
            panic!("{err:?}");
        };
        assert!(pinned.starts_with("SHA256:") && offered.starts_with("SHA256:"));
        assert_ne!(pinned, offered);
        assert!(!err.to_string().contains("ssh-ed25519"));
    }
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sftp_wrong_pinned_key_never_authenticates() {
    let sshd = SshdServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("other");
    assert!(
        Command::new("ssh-keygen")
            .args(["-q", "-N", "", "-t", "ed25519", "-f"])
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    let other = std::fs::read_to_string(dir.path().join("other.pub")).unwrap();
    let mark = sshd.log().len();
    let wrong = location(&sshd, SshUser::Srv, &other, &[]);
    let err = wrong.get("config").await.unwrap_err();
    assert!(matches!(err, BlobError::HostKeyMismatch { .. }), "{err:?}");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let log = sshd.log();
    assert!(
        !tail_after(&log, mark).contains("Accepted publickey"),
        "{log}"
    );
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sftp_ecdsa_key_authenticates() {
    let sshd = SshdServer::start().await;
    let server = store(&sshd, SshUser::Srv);
    server.ensure_prefixes(&["packs"]).await.unwrap();
    let key = sshd.private_key(SshUser::AgentEcdsa);
    let endpoint = sshd.endpoint();
    let host_key = sshd.host_key();
    let ecdsa = RepoLocation::new(
        "sftp",
        [
            ("endpoint", endpoint.as_str()),
            ("user", "agent"),
            ("key", key.as_str()),
            ("host_key", host_key.as_str()),
            ("root", sshd.root()),
        ],
    )
    .open(None)
    .unwrap();
    ecdsa
        .put_if_absent("packs/05/ecdsa", Bytes::from_static(b"ok"))
        .await
        .unwrap();
    assert_eq!(&server.get("packs/05/ecdsa").await.unwrap()[..], b"ok");
}
