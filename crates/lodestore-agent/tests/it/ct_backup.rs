#![cfg(target_os = "linux")]

use crate::common::{TestAgent, local_agent, short_socket_dir};

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lodestore_agent::jobs::JobHub;
use lodestore_agent::jobs::ct_backup::{PveCommands, PveJobs, translate_excludes};
use lodestore_agent::pve::install::embedded_plugin_hash;
use lodestore_agent::pve::socket::PveSessions;
use lodestore_agent::session::CAPABILITIES;
use lodestore_agent::socket;
use lodestore_format::ct::{CtMode, CtSnapshot};
use lodestore_format::fs::filter::PathMatcher;
use lodestore_format::pb;
use lodestore_format::reader::Reader;
use lodestore_format::repo::Repository;
use lodestore_format::source::{SourceKey, decode};
use lodestore_format::store::LocalFsStore;
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{
    AgentMessage, JobAssign, JobCancel, JobFinished, JobOutcome, JobSpec, SourceInstance,
    SourceSelector,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(120);
const UUID: &str = "0192f0c4-7d3a-7000-8000-000000000001";
const UID: &str = "abcdefghijklmnopqrst";
const NODE: &str = "pve1";
const JOB: &str = "job-ct";

fn proxmox_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/proxmox")
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A Proxmox VE node in temporary directories: `/etc/pve` with CT 200 on
/// this node, the installed plugin, the container's directory and a vzdump
/// that runs `fake-vzdump.pl` against the agent's own socket.
struct Lab {
    tmp: tempfile::TempDir,
    agent: TestAgent,
    hub: Arc<JobHub>,
    ct: PathBuf,
    pve: PathBuf,
    perl: PathBuf,
    shutdown: CancellationToken,
    _socket_dir: tempfile::TempDir,
}

impl Drop for Lab {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

impl Lab {
    async fn new() -> Lab {
        let tmp = tempfile::tempdir().unwrap();
        let agent = local_agent(&tmp.path().join("agent")).await;
        let (pve, perl) = (tmp.path().join("pve"), tmp.path().join("perl"));
        write(
            &pve.join(".members"),
            &json!({"nodename": NODE, "cluster": {"name": "lab"}, "nodelist": {NODE: {"online": 1}}})
                .to_string(),
        );
        write(
            &pve.join(format!("nodes/{NODE}/lxc/200.conf")),
            "arch: amd64\nhostname: ct200\n",
        );
        for file in [
            "PVE/Storage/Custom/LodestorePlugin.pm",
            "PVE/BackupProvider/Plugin/Lodestore.pm",
        ] {
            std::fs::create_dir_all(perl.join(file).parent().unwrap()).unwrap();
            std::fs::copy(proxmox_dir().join(file), perl.join(file)).unwrap();
        }
        write(
            &pve.join(format!("lodestore/plugin/{NODE}")),
            &format!("0.0.0 1 {} 9.3.1\n", embedded_plugin_hash()),
        );
        let ct = tmp.path().join("snap");
        write(&ct.join("etc/hostname"), "ct200\n");
        write(&ct.join("usr/bin/x"), "x");

        let proxmox = proxmox_dir();
        let vzdump = tmp.path().join("bin/vzdump");
        write(
            &vzdump,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{argv}'\n\
                 [ -n \"$LODESTORE_JOB_TOKEN\" ] && echo set > '{token}'\n\
                 LODESTORE_FAKE_VZDUMP='{scenario}' exec perl -T -I '{stubs}' -I '{lib}' '{fake}' \"$@\"\n",
                argv = tmp.path().join("argv").display(),
                token = tmp.path().join("token").display(),
                scenario = tmp.path().join("scenario.json").display(),
                stubs = proxmox.join("t/stubs").display(),
                lib = proxmox.display(),
                fake = proxmox.join("t/fake-vzdump.pl").display(),
            ),
        );
        std::fs::set_permissions(&vzdump, std::fs::Permissions::from_mode(0o755)).unwrap();

        let hub = JobHub::open(&agent.paths, &agent.agent_id).unwrap();
        let socket_dir = short_socket_dir();
        let socket_path = socket_dir.path().join("a.sock");
        let sessions = Arc::new(PveSessions::new(rustix::process::getuid().as_raw()));
        hub.set_pve(PveJobs {
            sessions: Arc::clone(&sessions),
            commands: PveCommands {
                vzdump,
                ..PveCommands::default()
            },
            pve_root: pve.clone(),
            perl_root: perl.clone(),
            socket: socket_path.clone(),
        });
        let shutdown = CancellationToken::new();
        tokio::spawn(socket::serve_with(
            Arc::clone(&hub),
            sessions,
            socket_path,
            shutdown.clone(),
        ));
        let lab = Lab {
            tmp,
            agent,
            hub,
            ct,
            pve,
            perl,
            shutdown,
            _socket_dir: socket_dir,
        };
        lab.record(UUID, UID);
        lab.scenario(json!({}));
        lab
    }

    fn record(&self, uuid: &str, uid: &str) {
        write(
            &self.agent.paths.source_ids(),
            &json!({"ct/200": {"uuid": uuid, "pve_uid": uid}}).to_string(),
        );
    }

    fn scenario(&self, extra: Value) {
        let mut scenario = json!({
            "dir": self.ct,
            "config": "arch: amd64\nhostname: ct200\n",
            "excludes": [],
            "mode": "snapshot",
            "sources": ["./"],
            "guest_uid": UID,
        });
        scenario
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        write(
            &self.tmp.path().join("scenario.json"),
            &scenario.to_string(),
        );
    }

    fn argv(&self) -> Option<Vec<String>> {
        let text = std::fs::read_to_string(self.tmp.path().join("argv")).ok()?;
        Some(text.lines().map(str::to_string).collect())
    }

    fn key() -> String {
        SourceKey::proxmox_ct("lab", UUID).unwrap().to_string()
    }

    fn assign(&self) {
        self.hub.handle(ServerBody::JobAssign(JobAssign {
            job_id: JOB.into(),
            spec: Some(JobSpec {
                policy_name: "ct".into(),
                source: Some(SourceSelector {
                    r#type: "proxmox-ct".into(),
                    vmids: vec![200],
                    ..Default::default()
                }),
                instance: Some(SourceInstance {
                    key: Lab::key(),
                    vmid: 200,
                }),
                ..Default::default()
            }),
            restore: None,
        }));
    }

    async fn wait(&self) -> Vec<AgentMessage> {
        let mut all = Vec::new();
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            all.extend(self.hub.outbound());
            if finished(&all).is_some() {
                return all;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the job did not finish"
            );
            tokio::select! {
                () = self.hub.changed() => {}
                () = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }
    }

    async fn run(&self) -> JobFinished {
        self.assign();
        finished(&self.wait().await).unwrap().clone()
    }

    async fn snapshot_count(&self) -> usize {
        crate::common::snapshots(&self.agent).await.len()
    }

    /// The one snapshot: its container metadata and every node below `/fs`.
    async fn container(&self) -> (pb::Snapshot, CtSnapshot, BTreeMap<String, pb::Node>) {
        let store = LocalFsStore::new(self.agent.repo_root.clone());
        let repo = Repository::open(&store, self.agent.password.as_bytes())
            .await
            .unwrap();
        let mut reader = Reader::open(&store, repo.read_keys().duplicate(), repo.config()).unwrap();
        let entries = reader.list_snapshots().await.unwrap().entries;
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        let ct = CtSnapshot::open(&mut reader, &entry.id).await.unwrap();
        let mut nodes = BTreeMap::new();
        let mut dirs = vec!["/fs".to_string()];
        while let Some(dir) = dirs.pop() {
            for node in reader.list_dir(&entry.id, dir.as_bytes()).await.unwrap() {
                let path = format!("{dir}/{}", String::from_utf8_lossy(&node.name));
                if node.r#type == pb::NodeType::Dir as i32 {
                    dirs.push(path.clone());
                }
                nodes.insert(path, node);
            }
        }
        (entry.snapshot.clone(), ct, nodes)
    }
}

fn finished(msgs: &[AgentMessage]) -> Option<&JobFinished> {
    msgs.iter().find_map(|m| match &m.body {
        Some(AgentBody::JobFinished(f)) => Some(f),
        _ => None,
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_backup_writes_ct_snapshot() {
    let lab = Lab::new().await;
    let done = lab.run().await;
    assert_eq!(done.outcome, JobOutcome::Succeeded as i32, "{done:?}");
    assert!(!done.snapshot_id.is_empty());

    let (snapshot, ct, nodes) = lab.container().await;
    assert_eq!(ct.meta.mode, CtMode::Snapshot);
    assert!(!ct.meta.unprivileged);
    assert_eq!(ct.config, "arch: amd64\nhostname: ct200\n");
    assert!(nodes.contains_key("/fs/etc/hostname"), "{nodes:?}");
    assert!(nodes.contains_key("/fs/usr/bin/x"));
    let (key, meta) = decode(snapshot.source.as_ref()).unwrap();
    assert_eq!(key.as_str(), Lab::key());
    assert_eq!(meta["vmid"], "200");
    assert_eq!(meta["name"], "ct200");
    assert_eq!(meta["ct_mode"], "snapshot");
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_backup_argv_is_fixed() {
    let lab = Lab::new().await;
    let done = lab.run().await;
    assert_eq!(done.outcome, JobOutcome::Succeeded as i32, "{done:?}");
    assert_eq!(
        lab.argv().unwrap(),
        [
            "200",
            "--storage",
            "lodestore",
            "--mode",
            "snapshot",
            "--remove",
            "0",
            "--protected",
            "0"
        ]
    );
    assert!(lab.tmp.path().join("token").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_backup_capability() {
    assert!(CAPABILITIES.contains(&"source:proxmox-ct"));
    let lab = Lab::new().await;
    let done = lab.run().await;
    assert_ne!(done.error_code, "source_agent_outdated", "{done:?}");
    assert_eq!(lab.snapshot_count().await, 1);
}

/// Fails with `code`, writes no snapshot, and says whether vzdump ran.
async fn refused(lab: &Lab, code: &str) -> (JobFinished, bool) {
    let done = lab.run().await;
    assert_eq!(done.outcome, JobOutcome::Failed as i32, "{done:?}");
    assert_eq!(done.error_code, code, "{done:?}");
    assert_eq!(lab.snapshot_count().await, 0);
    (done, lab.argv().is_some())
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_backup_not_local() {
    let lab = Lab::new().await;
    std::fs::create_dir_all(lab.pve.join("nodes/other/lxc")).unwrap();
    std::fs::rename(
        lab.pve.join(format!("nodes/{NODE}/lxc/200.conf")),
        lab.pve.join("nodes/other/lxc/200.conf"),
    )
    .unwrap();
    let (done, ran) = refused(&lab, "guest_not_local").await;
    assert!(!ran);
    assert_eq!(done.error_detail["vmid"], "200");
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_backup_uuid_mismatch() {
    let lab = Lab::new().await;
    lab.record("0192f0c4-7d3a-7000-8000-000000000002", UID);
    let (_, ran) = refused(&lab, "guest_identity_changed").await;
    assert!(!ran);
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_backup_guest_uid_changed() {
    for guest_uid in [json!("zyxwvutsrqponmlkjihg"), Value::Null] {
        let lab = Lab::new().await;
        lab.scenario(json!({ "guest_uid": guest_uid }));
        let (_, ran) = refused(&lab, "guest_identity_changed").await;
        assert!(ran, "the uid is checked when the provider opens the backup");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_backup_uid_appears_after_reuse() {
    let lab = Lab::new().await;
    lab.record(UUID, "");
    let (_, ran) = refused(&lab, "guest_identity_changed").await;
    assert!(ran);

    let lab = Lab::new().await;
    lab.record(UUID, "");
    lab.scenario(json!({ "guest_uid": null }));
    let done = lab.run().await;
    assert_eq!(done.outcome, JobOutcome::Succeeded as i32, "{done:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_backup_plugin_unhealthy() {
    let lab = Lab::new().await;
    let provider = lab.perl.join("PVE/BackupProvider/Plugin/Lodestore.pm");
    let mut text = std::fs::read_to_string(&provider).unwrap();
    text.push_str("# changed\n");
    std::fs::write(&provider, text).unwrap();
    let (done, ran) = refused(&lab, "guest_plugin_missing").await;
    assert!(!ran);
    assert!(
        done.error_message
            .contains(&format!("run lodestore-agent pve install on {NODE}")),
        "{}",
        done.error_message
    );
}

/// The paths of `tree` that survive `patterns`, as `tar` of vzdump keeps them
/// and as the walk keeps them with [`translate_excludes`].
fn kept_by_tar(
    tree: &Path,
    patterns: &[&str],
    archive: &Path,
) -> std::collections::BTreeSet<String> {
    let mut create = std::process::Command::new("tar");
    create.arg("-cf").arg(archive).arg("-C").arg(tree);
    create.arg("--no-anchored");
    create.args(
        patterns
            .iter()
            .filter(|p| !p.starts_with('/'))
            .map(|p| format!("--exclude={p}")),
    );
    create.arg("--anchored");
    create.args(
        patterns
            .iter()
            .filter(|p| p.starts_with('/'))
            .map(|p| format!("--exclude=.{p}")),
    );
    assert!(create.args(["--", "./"]).status().unwrap().success());
    let list = std::process::Command::new("tar")
        .arg("-tf")
        .arg(archive)
        .output()
        .unwrap();
    assert!(list.status.success());
    String::from_utf8(list.stdout)
        .unwrap()
        .lines()
        .map(|l| l.trim_start_matches("./").trim_end_matches('/').to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn kept_by_walk(
    dir: &Path,
    rel: &str,
    matcher: &PathMatcher,
    out: &mut std::collections::BTreeSet<String>,
) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        let path = format!("{rel}/{name}");
        let is_dir = entry.file_type().unwrap().is_dir();
        if matcher.is_match(path.as_bytes(), is_dir) {
            continue;
        }
        out.insert(path[1..].to_string());
        if is_dir {
            kept_by_walk(&entry.path(), &path, matcher, out);
        }
    }
}

#[test]
fn ct_backup_excludes_match_tar() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    for file in [
        "tmp/a",
        "tmp/sub/b",
        "var/tmp/c",
        "var/run/x.pid",
        "var/run/keep",
        "home/u/b.cache",
        "home/u/.cache/d",
        "srv/tmp/e",
        "data/logs/old.log",
    ] {
        write(&tree.join(file), "x");
    }
    let patterns = [
        "/tmp/?*",
        "/var/tmp/?*",
        "/var/run/?*.pid",
        "*.cache",
        "/data/logs/*.log",
        "tmp",
        "*/sub/*",
        "/srv/",
    ];
    let sets: Vec<Vec<&str>> = patterns
        .iter()
        .map(|p| vec![*p])
        .chain([patterns.to_vec(), Vec::new()])
        .collect();
    for set in sets {
        let owned: Vec<String> = set.iter().map(|p| p.to_string()).collect();
        let (matcher, _) = translate_excludes(&owned);
        let mut walked = std::collections::BTreeSet::new();
        kept_by_walk(&tree, "", &matcher, &mut walked);
        let tar = kept_by_tar(&tree, &set, &tmp.path().join("a.tar"));
        assert_eq!(walked, tar, "patterns {set:?}");
    }
    let (_, warnings) = translate_excludes(&["/srv/".to_string()]);
    assert_eq!(warnings.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_backup_mode_from_output() {
    let lab = Lab::new().await;
    let copy = lab.tmp.path().join("tmp/vzdumptmp4242_200");
    write(&copy.join("etc/hostname"), "ct200\n");
    lab.scenario(json!({ "dir": copy, "mode": "suspend", "sources": ["."] }));
    let done = lab.run().await;
    assert_eq!(done.outcome, JobOutcome::Succeeded as i32, "{done:?}");
    let (_, ct, _) = lab.container().await;
    assert_eq!(ct.meta.mode, CtMode::Suspend);

    let lab = Lab::new().await;
    let copy = lab.tmp.path().join("tmp/vzdumptmp4242_200");
    write(&copy.join("etc/hostname"), "ct200\n");
    lab.scenario(json!({ "dir": copy, "mode": "snapshot", "sources": ["."] }));
    refused(&lab, "ct_mode_mismatch").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_backup_excludes() {
    let lab = Lab::new().await;
    for file in ["tmp/a", "var/run/x.pid", "var/run/keep", "home/u/b.cache"] {
        write(&lab.ct.join(file), "x");
    }
    lab.scenario(json!({ "excludes": ["/tmp/?*", "/var/tmp/?*", "/var/run/?*.pid", "*.cache"] }));
    let done = lab.run().await;
    assert_eq!(done.outcome, JobOutcome::Succeeded as i32, "{done:?}");
    let (_, _, nodes) = lab.container().await;
    for gone in ["/fs/tmp/a", "/fs/var/run/x.pid", "/fs/home/u/b.cache"] {
        assert!(!nodes.contains_key(gone), "{gone}");
    }
    for kept in ["/fs/tmp", "/fs/var/run/keep", "/fs/home/u"] {
        assert!(nodes.contains_key(kept), "{kept}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_backup_mount_points() {
    let lab = Lab::new().await;
    write(&lab.ct.join("data/f"), "mounted");
    lab.scenario(json!({ "sources": ["./", "./data"] }));
    let done = lab.run().await;
    assert_eq!(done.outcome, JobOutcome::Succeeded as i32, "{done:?}");
    let (_, _, nodes) = lab.container().await;
    assert!(nodes.contains_key("/fs/data/f"));

    let lab = Lab::new().await;
    std::os::unix::fs::symlink(lab.tmp.path(), lab.ct.join("outside")).unwrap();
    lab.scenario(json!({ "sources": ["./", "./outside"] }));
    refused(&lab, "source_invalid").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_backup_empty_rootfs() {
    let lab = Lab::new().await;
    std::fs::remove_dir_all(lab.ct.join("etc")).unwrap();
    refused(&lab, "ct_empty_rootfs").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_backup_fail_after_data() {
    let lab = Lab::new().await;
    lab.scenario(json!({ "fail_after_data": true }));
    let (done, _) = refused(&lab, "vzdump_failed").await;
    assert!(
        done.error_message.contains("failure injected"),
        "{}",
        done.error_message
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_backup_cancel() {
    let lab = Lab::new().await;
    let term = lab.tmp.path().join("terminated");
    lab.scenario(json!({ "pause": 60, "term_file": term }));
    lab.assign();
    let started = tokio::time::Instant::now();
    while lab.argv().is_none() {
        assert!(started.elapsed() < WAIT, "vzdump did not start");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    lab.hub
        .handle(ServerBody::JobCancel(JobCancel { job_id: JOB.into() }));
    let msgs = lab.wait().await;
    let done = finished(&msgs).unwrap();
    assert_eq!(done.outcome, JobOutcome::Cancelled as i32, "{done:?}");
    assert!(started.elapsed() < Duration::from_secs(30));
    assert!(term.exists(), "vzdump got no SIGTERM");
    assert_eq!(lab.snapshot_count().await, 0);
}

fn caps_v3(rootid: u32) -> Vec<u8> {
    let mut out = 0x0300_0001u32.to_le_bytes().to_vec();
    out.extend_from_slice(&(1u32 << 13).to_le_bytes());
    out.extend_from_slice(&[0; 12]);
    out.extend_from_slice(&rootid.to_le_bytes());
    out
}

/// `(tag, perm, id)` of each entry of a POSIX ACL xattr.
fn acl_entries(blob: &[u8]) -> Vec<(u16, u16, u32)> {
    blob.get(4..)
        .unwrap_or_default()
        .chunks(8)
        .map(|e| {
            (
                u16::from_le_bytes([e[0], e[1]]),
                u16::from_le_bytes([e[2], e[3]]),
                u32::from_le_bytes([e[4], e[5], e[6], e[7]]),
            )
        })
        .collect()
}

fn xattr<'a>(node: &'a pb::Node, name: &str) -> Option<&'a [u8]> {
    node.xattrs
        .iter()
        .find(|x| x.name == name.as_bytes())
        .map(|x| x.value.as_slice())
}

/// The container of an unprivileged CT as root on the node sees it: host
/// ids 100000 + n, an ACL entry for 101000, a version 3 capability with
/// root id 100000 and one file whose owner is outside the default map.
fn host_view(ct: &Path) {
    use std::os::unix::fs::{chown, lchown};

    write(&ct.join("etc/shadow"), "root:*:1::::::\n");
    write(&ct.join("var/www/index.html"), "hi");
    write(&ct.join("home/u/doc"), "doc");
    write(&ct.join("usr/bin/ping"), "elf");
    write(&ct.join("stray"), "outside the map");
    let mut dirs = vec![ct.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        lchown(&dir, Some(100_000), Some(100_000)).unwrap();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            lchown(entry.path(), Some(100_000), Some(100_000)).unwrap();
            if entry.file_type().unwrap().is_dir() {
                dirs.push(entry.path());
            }
        }
    }
    chown(ct.join("etc/shadow"), Some(100_000), Some(100_042)).unwrap();
    std::fs::set_permissions(
        ct.join("etc/shadow"),
        std::fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    chown(ct.join("var/www/index.html"), Some(100_033), Some(100_033)).unwrap();
    chown(ct.join("home/u/doc"), Some(101_000), Some(101_000)).unwrap();
    chown(ct.join("stray"), Some(200_000), Some(200_000)).unwrap();
    let status = std::process::Command::new("setfacl")
        .args(["-m", "u:101000:rw"])
        .arg(ct.join("home/u/doc"))
        .status()
        .expect("setfacl is needed for this test");
    assert!(status.success());
    rustix::fs::setxattr(
        ct.join("usr/bin/ping"),
        "security.capability",
        &caps_v3(100_000),
        rustix::fs::XattrFlags::empty(),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs root"]
async fn pve_root_ct_backup_unprivileged_default_map() {
    let lab = Lab::new().await;
    host_view(&lab.ct);
    lab.scenario(json!({ "config": "arch: amd64\nhostname: ct200\nunprivileged: 1\n" }));
    let done = lab.run().await;
    assert_eq!(done.outcome, JobOutcome::Warnings as i32, "{done:?}");
    assert!(done.warnings_total >= 1);

    let (_, ct, nodes) = lab.container().await;
    assert!(ct.meta.unprivileged);
    assert_eq!(
        ct.meta.idmap.unwrap().canonical().unwrap(),
        "u:0:100000:65536,g:0:100000:65536"
    );
    let ids = |path: &str| (nodes[path].uid, nodes[path].gid);
    assert_eq!(ids("/fs/etc/hostname"), (0, 0));
    assert_eq!(ids("/fs/etc/shadow"), (0, 42));
    assert_eq!(nodes["/fs/etc/shadow"].mode & 0o7777, 0o640);
    assert_eq!(ids("/fs/var/www/index.html"), (33, 33));
    assert_eq!(ids("/fs/home/u/doc"), (1000, 1000));
    assert_eq!(ids("/fs/stray"), (65534, 65534));
    let acl = acl_entries(xattr(&nodes["/fs/home/u/doc"], "system.posix_acl_access").unwrap());
    assert!(acl.contains(&(0x02, 6, 1000)), "{acl:?}");
    assert_eq!(
        xattr(&nodes["/fs/usr/bin/ping"], "security.capability").map(<[u8]>::len),
        Some(20),
        "a version 2 capability in the container's view"
    );
    assert!(
        nodes
            .values()
            .all(|n| n.user.is_empty() && n.group.is_empty())
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs root"]
async fn pve_root_ct_backup_privileged_ids() {
    let lab = Lab::new().await;
    host_view(&lab.ct);
    let done = lab.run().await;
    assert_eq!(done.outcome, JobOutcome::Succeeded as i32, "{done:?}");

    let (snapshot, ct, nodes) = lab.container().await;
    assert!(!ct.meta.unprivileged);
    assert!(ct.meta.idmap.is_none());
    let (_, meta) = decode(snapshot.source.as_ref()).unwrap();
    assert!(!meta.contains_key("ct_idmap"));
    assert_eq!(
        (nodes["/fs/etc/shadow"].uid, nodes["/fs/etc/shadow"].gid),
        (100_000, 100_042)
    );
    assert_eq!(nodes["/fs/stray"].uid, 200_000);
    assert_eq!(
        xattr(&nodes["/fs/usr/bin/ping"], "security.capability"),
        Some(caps_v3(100_000).as_slice())
    );
    assert!(
        nodes
            .values()
            .all(|n| n.user.is_empty() && n.group.is_empty())
    );
}
