#![cfg(target_os = "linux")]

use crate::common::{TestAgent, local_agent, short_socket_dir};

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lodestore_agent::jobs::JobHub;
use lodestore_agent::jobs::ct_backup::{PveCommands, PveJobs};
use lodestore_agent::jobs::fetch::{FetchError, PackAccess, PackFetcher};
use lodestore_agent::jobs::restore::{PlanError, PlanSource, RestoreOptions, run_restore_with};
use lodestore_agent::pve::install::embedded_plugin_hash;
use lodestore_agent::pve::socket::PveSessions;
use lodestore_agent::session::CAPABILITIES;
use lodestore_agent::socket;
use lodestore_format::crypto::BlobId;
use lodestore_format::ct::{CtMeta, CtMode};
use lodestore_format::fs::restore::SpaceReserve;
use lodestore_format::pack::pack_object_key;
use lodestore_format::pb::{Node, NodeType};
use lodestore_format::reader::Reader;
use lodestore_format::repo::Repository;
use lodestore_format::selection::Selection;
use lodestore_format::source::{Meta, SourceKey};
use lodestore_format::store::{BlobStore, LocalFsStore};
use lodestore_format::tree::{PlanStep, SelectionWalk};
use lodestore_format::writer::{BackupOptions, CtBackup, Writer};
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::plan_item::Kind;
use lodestore_proto::agent::v1::restore_spec::Target;
use lodestore_proto::agent::v1::{
    AgentMessage, ChunkLoc, DatabaseTarget, GuestRestoreResult, GuestTarget, JobFinished,
    JobOutcome, PlanEnter, PlanEntry, PlanItem, PlanLeave, RestorePlanBatch, RestoreSpec,
    RestoreTotals,
};
use prost::Message;
use rustix::fs::{FileType, Mode, OFlags};
use serde_json::json;
use tokio_util::sync::CancellationToken;

pub(crate) const NODE: &str = "pve1";
const UUID: &str = "0192f0c4-7d3a-7000-8000-000000000001";
const WAIT: Duration = Duration::from_secs(120);
const PAUSE: Duration = Duration::from_millis(10);

pub(crate) fn proxmox_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/proxmox")
}

pub(crate) fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// A Proxmox VE node in temporary directories: `/etc/pve` with storages and
/// the installed plugin, `/sys` with bridge `vmbr0`, and a `pct` that runs
/// `fake-pct.pl` against the agent's own socket.
pub(crate) struct Lab {
    pub tmp: tempfile::TempDir,
    pub agent: TestAgent,
    pub hub: Arc<JobHub>,
    pub pve: PathBuf,
    pub sys: PathBuf,
    shutdown: CancellationToken,
    _socket_dir: tempfile::TempDir,
    next: AtomicUsize,
}

impl Drop for Lab {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// What the server sends for `/fs` and `/config` of a snapshot.
pub(crate) struct Snap {
    pub repo_id: [u8; 16],
    pub k_id: [u8; 32],
    pub batches: Vec<RestorePlanBatch>,
}

/// What one restore left behind.
pub(crate) struct Ran {
    pub done: JobFinished,
    pub msgs: Vec<AgentMessage>,
    /// `pct`'s arguments; `None` when it did not run.
    pub argv: Option<Vec<String>>,
    /// The configuration Proxmox VE got.
    pub config: Option<String>,
    pub firewall: bool,
    /// The staging folder as `pct` saw it.
    pub copy: PathBuf,
}

impl Ran {
    pub fn guest(&self) -> &GuestRestoreResult {
        self.done
            .restore
            .as_ref()
            .and_then(|r| r.guest.as_ref())
            .unwrap_or_else(|| panic!("no guest result: {:?}", self.done))
    }
}

impl Lab {
    pub async fn new() -> Lab {
        let tmp = tempfile::tempdir().unwrap();
        let agent = local_agent(&tmp.path().join("agent")).await;
        let (pve, perl, sys) = (
            tmp.path().join("pve"),
            tmp.path().join("perl"),
            tmp.path().join("sys"),
        );
        write(
            &pve.join(".members"),
            &json!({"nodename": NODE, "cluster": {"name": "lab"}, "nodelist": {NODE: {"online": 1}}})
                .to_string(),
        );
        write(
            &pve.join("storage.cfg"),
            "dir: local\n\tpath /var/lib/vz\n\tcontent iso,vztmpl,backup\n\n\
             lvmthin: local-lvm\n\tthinpool data\n\tvgname pve\n\tcontent rootdir,images\n",
        );
        for file in [
            "PVE/Storage/Custom/LodestorePlugin.pm",
            "PVE/BackupProvider/Plugin/Lodestore.pm",
        ] {
            fs::create_dir_all(perl.join(file).parent().unwrap()).unwrap();
            fs::copy(proxmox_dir().join(file), perl.join(file)).unwrap();
        }
        write(
            &pve.join(format!("lodestore/plugin/{NODE}")),
            &format!("0.0.0 1 {} 9.3.1\n", embedded_plugin_hash()),
        );
        fs::create_dir_all(pve.join(format!("nodes/{NODE}/lxc"))).unwrap();
        fs::create_dir_all(sys.join("class/net/vmbr0/bridge")).unwrap();
        fs::create_dir_all(sys.join("class/net/eno1")).unwrap();

        let hub = JobHub::open_with_tick(&agent.paths, &agent.agent_id, PAUSE).unwrap();
        let socket_dir = short_socket_dir();
        let socket_path = socket_dir.path().join("a.sock");
        let sessions = Arc::new(PveSessions::new(rustix::process::getuid().as_raw()));
        hub.set_pve(PveJobs {
            sessions: Arc::clone(&sessions),
            commands: PveCommands {
                pct: tmp.path().join("bin/pct"),
                ..PveCommands::default()
            },
            pve_root: pve.clone(),
            perl_root: perl,
            sys_root: sys.clone(),
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
            pve,
            sys,
            shutdown,
            _socket_dir: socket_dir,
            next: AtomicUsize::new(0),
        };
        lab.vmlist(&[]);
        lab.pct_script(None);
        lab
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.tmp.path().join(rel)
    }

    pub fn vmlist(&self, vmids: &[u32]) {
        let ids: serde_json::Map<String, serde_json::Value> = vmids
            .iter()
            .map(|v| {
                (
                    v.to_string(),
                    json!({"node": NODE, "type": "lxc", "version": 1}),
                )
            })
            .collect();
        write(
            &self.pve.join(".vmlist"),
            &json!({"version": 1, "ids": ids}).to_string(),
        );
    }

    /// `pct` runs `fake-pct.pl`, or prints `fail` and exits 1.
    pub fn pct_script(&self, fail: Option<&str>) {
        let proxmox = proxmox_dir();
        let body = match fail {
            Some(text) => format!("echo '{text}' >&2\nexit 1\n"),
            None => format!(
                "LODESTORE_FAKE_PCT='{scenario}' exec perl -I '{stubs}' -I '{lib}' '{fake}' \"$@\"\n",
                scenario = self.path("pct.json").display(),
                stubs = proxmox.join("t/stubs").display(),
                lib = proxmox.display(),
                fake = proxmox.join("t/fake-pct.pl").display(),
            ),
        };
        let pct = self.path("bin/pct");
        write(&pct, &format!("#!/bin/sh\n{body}"));
        fs::set_permissions(&pct, fs::Permissions::from_mode(0o755)).unwrap();
    }

    pub fn agent_yaml(&self, yaml: &str) {
        let file = self.agent.paths.agent_yaml();
        write(&file, yaml);
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    }

    pub fn staging_default(&self) -> PathBuf {
        self.agent.paths.state_dir.join("pve-staging")
    }

    /// A CT snapshot with `config` and `firewall`, its `/fs` built by
    /// `tree`, and the plan the server sends for it. `inject` puts more
    /// entries into the folder of that name.
    pub async fn snapshot(
        &self,
        config: &str,
        firewall: Option<&str>,
        tree: impl FnOnce(&Path),
        inject: &[(&str, Node)],
    ) -> Snap {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let root = self.path(&format!("ct-{n}"));
        fs::create_dir_all(root.join("etc")).unwrap();
        fs::write(root.join("etc/hostname"), "ct200\n").unwrap();
        tree(&root);
        let store = LocalFsStore::new(self.agent.repo_root.clone());
        let repo = Repository::open(&store, self.agent.password.as_bytes())
            .await
            .unwrap();
        let keys = repo.read_keys().write_keys(repo.config_hash());
        let mut writer = Writer::new(&store, keys, repo.config()).await.unwrap();
        let dir = rustix::fs::open(
            &root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        let ct = CtBackup {
            key: SourceKey::proxmox_ct("lab", UUID).unwrap(),
            meta: CtMeta::from_config(CtMode::Snapshot, config).unwrap(),
            source_meta: Meta::from([("vmid".to_owned(), "200".to_owned())]),
            config: config.to_owned(),
            firewall: firewall.map(str::to_owned),
            exclude: None,
        };
        let pending = writer
            .backup_container(&dir, ct, BackupOptions::default(), &|_| {})
            .await
            .unwrap();
        let id = pending.commit(&mut writer).await.unwrap().snapshot_id;
        drop(writer);

        let mut reader = Reader::open(&store, repo.read_keys().duplicate(), repo.config()).unwrap();
        let ids = (*reader.keys().repo_id(), *reader.keys().k_id());
        let selection = Selection::from_paths([b"/fs".to_vec(), b"/config".to_vec()]).unwrap();
        let mut walk = SelectionWalk::start(&mut reader, &id, &selection)
            .await
            .unwrap();
        let mut packs = Vec::new();
        let mut index: HashMap<BlobId, u32> = HashMap::new();
        let mut items = Vec::new();
        let (mut files, mut bytes) = (0u64, 0u64);
        let entry = |node: &Node, chunks: Vec<ChunkLoc>| PlanItem {
            kind: Some(Kind::Entry(PlanEntry {
                node: node.encode_to_vec(),
                chunks,
                more: false,
            })),
        };
        while let Some(step) = walk.next(&mut reader).await.unwrap() {
            match step {
                PlanStep::Enter { mut node, ancestor } => {
                    node.subtrees.clear();
                    items.push(PlanItem {
                        kind: Some(Kind::Enter(PlanEnter {
                            node: node.encode_to_vec(),
                            ancestor,
                        })),
                    });
                    for (_, extra) in inject.iter().filter(|(d, _)| d.as_bytes() == node.name) {
                        items.push(entry(extra, Vec::new()));
                    }
                }
                PlanStep::Leave => items.push(PlanItem {
                    kind: Some(Kind::Leave(PlanLeave {})),
                }),
                PlanStep::Entry(mut node) => {
                    let mut locs = Vec::new();
                    if node.r#type == NodeType::File as i32 {
                        files += 1;
                        bytes += node.size;
                        let found = reader.chunk_locations(&node).await.unwrap();
                        for (cid, loc) in node.content.iter().zip(&found) {
                            let pack = match index.get(&loc.pack_id) {
                                Some(p) => *p,
                                None => {
                                    let pack = packs.len() as u32;
                                    let key = reader.pack_key(&loc.pack_id).await.unwrap();
                                    packs.push(lodestore_proto::agent::v1::PackAccess {
                                        pack,
                                        pack_id: loc.pack_id.to_vec(),
                                        key: key.to_vec(),
                                        ..Default::default()
                                    });
                                    index.insert(loc.pack_id, pack);
                                    pack
                                }
                            };
                            locs.push(ChunkLoc {
                                pack,
                                offset: loc.offset,
                                enc_len: loc.enc_len,
                                ulen: loc.ulen,
                                id: cid.clone(),
                            });
                        }
                    }
                    node.content.clear();
                    node.subtrees.clear();
                    items.push(entry(&node, locs));
                }
            }
        }
        let batches = vec![RestorePlanBatch {
            items,
            packs,
            totals: Some(RestoreTotals { files, bytes }),
            last: true,
        }];
        Snap {
            repo_id: ids.0,
            k_id: ids.1,
            batches,
        }
    }

    pub fn spec(&self, snap: &Snap, guest: GuestTarget) -> RestoreSpec {
        RestoreSpec {
            snapshot_id: "ct-snapshot".into(),
            source_repo_id: snap.repo_id.to_vec(),
            k_id: snap.k_id.to_vec(),
            target: Some(Target::Guest(guest)),
            guest_snapshot: true,
            ..Default::default()
        }
    }

    pub async fn run(&self, snap: &Snap, spec: RestoreSpec) -> Ran {
        self.run_with(
            snap,
            spec,
            SpaceReserve {
                min_bytes: 0,
                percent: 0,
            },
        )
        .await
    }

    pub async fn run_with(&self, snap: &Snap, spec: RestoreSpec, reserve: SpaceReserve) -> Ran {
        let vmid = match &spec.target {
            Some(Target::Guest(g)) => g.vmid,
            _ => 0,
        };
        let copy = self.path("copy");
        let _ = fs::remove_dir_all(&copy);
        fs::create_dir_all(&copy).unwrap();
        for f in ["argv", "fw"] {
            let _ = fs::remove_file(self.path(f));
        }
        let out_config = self.pve.join(format!("nodes/{NODE}/lxc/{vmid}.conf"));
        write(
            &self.path("pct.json"),
            &json!({"out_config": out_config, "out_argv": self.path("argv"),
                "out_fw": self.path("fw"), "copy_to": copy})
            .to_string(),
        );
        let id = format!("restore-{}", self.next.fetch_add(1, Ordering::Relaxed));
        let plan = MemPlan(snap.batches.clone(), 0);
        let fetch = StoreFetch(LocalFsStore::new(self.agent.repo_root.clone()));
        let options = RestoreOptions {
            plan_pauses: vec![PAUSE; 3],
            space_reserve: reserve,
            ..Default::default()
        };
        self.hub
            .start_restore_with(id.clone(), spec, move |ctx, spec| {
                run_restore_with(&ctx, spec, plan, fetch, &options);
            });
        let msgs = self.wait(&id).await;
        let done = finished(&msgs).unwrap().clone();
        let argv = fs::read(self.path("argv"))
            .ok()
            .map(|b| serde_json::from_slice(&b).unwrap());
        let config = argv
            .as_ref()
            .and_then(|_: &Vec<String>| fs::read_to_string(&out_config).ok());
        Ran {
            done,
            msgs,
            argv,
            config,
            firewall: self.path("fw").exists(),
            copy,
        }
    }

    async fn wait(&self, id: &str) -> Vec<AgentMessage> {
        let mut all = Vec::new();
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            all.extend(self.hub.outbound());
            if finished(&all).is_some_and(|f| f.job_id == id) {
                return all;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{id} did not finish"
            );
            tokio::select! {
                () = self.hub.changed() => {}
                () = tokio::time::sleep(PAUSE) => {}
            }
        }
    }

    /// Nothing is left in the staging folder.
    pub fn staging_clean(&self, base: &Path) -> bool {
        fs::read_dir(base).map_or(true, |mut d| d.next().is_none())
    }
}

pub(crate) fn finished(msgs: &[AgentMessage]) -> Option<&JobFinished> {
    msgs.iter().find_map(|m| match &m.body {
        Some(AgentBody::JobFinished(f)) => Some(f),
        _ => None,
    })
}

pub(crate) struct MemPlan(pub Vec<RestorePlanBatch>, pub usize);

impl PlanSource for MemPlan {
    async fn next(&mut self) -> Result<Option<RestorePlanBatch>, PlanError> {
        self.1 += 1;
        Ok(self.0.get(self.1 - 1).cloned())
    }

    async fn reopen(&mut self) -> Result<(), PlanError> {
        self.1 = 0;
        Ok(())
    }
}

pub(crate) struct StoreFetch(pub LocalFsStore);

impl PackFetcher for StoreFetch {
    async fn fetch(
        &self,
        access: &PackAccess,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, FetchError> {
        let id = BlobId::try_from(access.pack_id.as_slice()).unwrap();
        self.0
            .get_range(&pack_object_key(&id), offset..offset + len)
            .await
            .map(|b| b.to_vec())
            .map_err(|e| FetchError::Failed(e.to_string()))
    }

    async fn refresh(&self, _: &[u32]) -> Result<Vec<PackAccess>, FetchError> {
        Err(FetchError::Failed("no refresh in this test".into()))
    }
}

pub(crate) fn new_ct(vmid: u32) -> GuestTarget {
    GuestTarget {
        node: NODE.into(),
        vmid,
        storage: "local-lvm".into(),
        ..Default::default()
    }
}

fn snapshot_config(uid: Option<&str>, extra: &str) -> String {
    let meta = uid.map_or(String::new(), |u| {
        format!("meta: ctime=1600000000,uid={u},rtime=3\n")
    });
    format!(
        "arch: amd64\nhostname: ct200\n{meta}lxc.apparmor.profile: unconfined\nnet0: name=eth0,bridge=vmbr0,hwaddr=AA:BB:CC:00:00:01,type=veth\nrootfs: local-lvm:vm-200-disk-0,size=8G\n{extra}"
    )
}

/// Succeeded; as a regular user the root-owned `/config` of the snapshot
/// keeps the user as owner, which is a warning.
pub(crate) fn succeeded(ran: &Ran) {
    let ok = if rustix::process::geteuid().is_root() {
        vec![JobOutcome::Succeeded as i32]
    } else {
        vec![JobOutcome::Succeeded as i32, JobOutcome::Warnings as i32]
    };
    assert!(ok.contains(&ran.done.outcome), "{:?}", ran.msgs);
}

fn failed_with(ran: &Ran, code: &str) {
    assert_eq!(
        ran.done.outcome,
        JobOutcome::Failed as i32,
        "{:?}",
        ran.done
    );
    assert_eq!(ran.done.error_code, code, "{:?}", ran.done);
}

fn new_argv(vmid: u32, extra: &[&str]) -> Vec<String> {
    let mut argv: Vec<String> = [
        "restore",
        &vmid.to_string(),
        "",
        "--unique",
        "1",
        "--start",
        "0",
        "--unprivileged",
        "1",
        "--onboot",
        "0",
        "--storage",
        "local-lvm",
    ]
    .map(String::from)
    .to_vec();
    argv.extend(extra.iter().map(|s| s.to_string()));
    argv
}

/// `argv` with the volume id checked and blanked.
fn argv_shape(ran: &Ran) -> Vec<String> {
    let mut argv = ran.argv.clone().expect("pct ran");
    let volid = argv[2].clone();
    let id = volid
        .strip_prefix("lodestore:backup/restore/")
        .unwrap_or_else(|| panic!("{volid}"));
    assert!(
        id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "{id}"
    );
    argv[2] = String::new();
    argv
}

fn dropped(ran: &Ran) -> Vec<(String, String)> {
    ran.guest()
        .dropped
        .iter()
        .map(|d| (d.key.clone(), d.reason.clone()))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_restore_new_vmid() {
    let lab = Lab::new().await;
    let snap = lab
        .snapshot(&snapshot_config(None, ""), None, |_| {}, &[])
        .await;
    let ran = lab.run(&snap, lab.spec(&snap, new_ct(105))).await;
    succeeded(&ran);
    let argv = argv_shape(&ran);
    assert_eq!(argv, new_argv(105, &[]));
    assert!(!argv.iter().any(|a| a == "--ha-managed"));
    let config = ran.config.as_deref().unwrap();
    assert!(!config.contains("lxc.apparmor"), "{config}");
    assert!(config.contains("unprivileged: 1\n") && config.contains("onboot: 0\n"));
    assert_eq!(
        fs::read_to_string(ran.copy.join("etc/hostname")).unwrap(),
        "ct200\n"
    );
    let guest = ran.guest();
    assert_eq!(
        (guest.vmid, guest.node.as_str(), guest.storage.as_str()),
        (105, NODE, "local-lvm")
    );
    assert!(!guest.replaced);
    assert_eq!(
        dropped(&ran),
        [("lxc.apparmor.profile".to_string(), "lxc".to_string())]
    );
    assert_eq!(guest.network.len(), 1);
    assert_eq!(guest.network[0].key, "net0");
    assert!(guest.network[0].value.contains("bridge=vmbr0"));
    assert_eq!(guest.new_mac_nets, ["net0"]);
    assert!(guest.unprivileged);
    assert_eq!(guest.root_host_uid, 100_000);
    assert_eq!(guest.name, "ct200");
    assert!(lab.staging_clean(&lab.staging_default()));
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_restore_capability() {
    assert!(CAPABILITIES.contains(&"guest_restore_ct_v1"));
    let lab = Lab::new().await;
    let snap = lab
        .snapshot(&snapshot_config(None, ""), None, |_| {}, &[])
        .await;
    let spec = RestoreSpec {
        target: Some(Target::Database(DatabaseTarget {
            database: "app".into(),
            in_place: false,
        })),
        ..lab.spec(&snap, new_ct(105))
    };
    let ran = lab.run(&snap, spec).await;
    failed_with(&ran, "restore_failed");
    assert!(
        ran.done
            .error_message
            .contains("unsupported restore target")
    );
    assert!(ran.argv.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_restore_no_firewall_no_devices() {
    let lab = Lab::new().await;
    let sda = Node {
        name: b"sda".to_vec(),
        r#type: NodeType::Blockdev as i32,
        mode: 0o660,
        rdev: rustix::fs::makedev(8, 0),
        ..Default::default()
    };
    let snap = lab
        .snapshot(
            &snapshot_config(None, ""),
            Some("[OPTIONS]\nenable: 1\n"),
            |root| {
                fs::create_dir_all(root.join("dev")).unwrap();
                let spool = root.join("var/spool/postfix/public");
                fs::create_dir_all(&spool).unwrap();
                rustix::fs::mknodat(
                    rustix::fs::CWD,
                    spool.join("pickup"),
                    FileType::Fifo,
                    Mode::RUSR | Mode::WUSR,
                    0,
                )
                .unwrap();
            },
            &[("dev", sda)],
        )
        .await;
    let ran = lab.run(&snap, lab.spec(&snap, new_ct(105))).await;
    assert_ne!(
        ran.done.outcome,
        JobOutcome::Failed as i32,
        "{:?}",
        ran.msgs
    );
    assert!(ran.copy.join("dev").is_dir());
    assert!(!ran.copy.join("dev/sda").exists());
    let pickup = fs::symlink_metadata(ran.copy.join("var/spool/postfix/public/pickup")).unwrap();
    assert!(pickup.file_type().is_fifo());
    assert_eq!(ran.done.restore.as_ref().unwrap().special_skipped, 1);
    assert!(dropped(&ran).contains(&("pct.fw".to_string(), "firewall".to_string())));
    assert!(!ran.firewall);
    assert!(lab.staging_clean(&lab.staging_default()));
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_restore_bridge_missing() {
    let lab = Lab::new().await;
    let config = snapshot_config(None, "net1: name=eth1,bridge=vmbr9\n");
    let snap = lab.snapshot(&config, None, |_| {}, &[]).await;
    let ran = lab.run(&snap, lab.spec(&snap, new_ct(105))).await;
    succeeded(&ran);
    let config = ran.config.as_deref().unwrap();
    assert!(!config.contains("net1"), "{config}");
    assert!(config.contains("net0: name=eth0,bridge=vmbr0"), "{config}");
    assert!(dropped(&ran).contains(&("net1".to_string(), "network".to_string())));
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_restore_staging_dir_setting() {
    let lab = Lab::new().await;
    let snap = lab
        .snapshot(&snapshot_config(None, ""), None, |_| {}, &[])
        .await;
    lab.agent_yaml("restore:\n  guest_staging_dir: /etc/pve/x\n");
    let ran = lab.run(&snap, lab.spec(&snap, new_ct(105))).await;
    failed_with(&ran, "restore_staging_invalid");
    assert!(ran.argv.is_none());
    assert!(!lab.staging_default().exists());
    assert!(!Path::new("/etc/pve/x").exists());

    let custom = lab.path("staging-here/nested");
    lab.agent_yaml(&format!(
        "restore:\n  guest_staging_dir: {}\n",
        custom.display()
    ));
    let ran = lab.run(&snap, lab.spec(&snap, new_ct(105))).await;
    succeeded(&ran);
    assert!(custom.is_dir());
    assert_eq!(
        fs::metadata(&custom).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(lab.staging_clean(&custom));
    assert!(!lab.staging_default().exists());
}
