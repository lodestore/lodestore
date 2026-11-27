#![cfg(target_os = "linux")]

use crate::common::{TestAgent, local_agent, short_socket_dir};

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lodestore_agent::jobs::JobHub;
use lodestore_agent::jobs::ct_backup::{PveCommands, PveJobs};
use lodestore_agent::pve::install::embedded_plugin_hash;
use lodestore_agent::pve::socket::PveSessions;
use lodestore_agent::session::CAPABILITIES;
use lodestore_agent::socket;
use lodestore_format::ct::{CtMode, CtSnapshot};
use lodestore_format::pb;
use lodestore_format::reader::Reader;
use lodestore_format::repo::Repository;
use lodestore_format::source::{SourceKey, decode};
use lodestore_format::store::LocalFsStore;
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{
    AgentMessage, JobAssign, JobFinished, JobOutcome, JobSpec, SourceInstance, SourceSelector,
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
