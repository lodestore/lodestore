#![cfg(target_os = "linux")]

use crate::common::{TestAgent, local_agent, short_socket_dir};

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lodestore_agent::jobs::JobHub;
use lodestore_agent::jobs::ct_backup::{PveCommands, PveJobs};
use lodestore_agent::jobs::ct_restore::clean_stale_staging;
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
    AgentMessage, ChunkLoc, ConfigLine, DatabaseTarget, ExpectedGuest, GuestRestoreResult,
    GuestTarget, JobFinished, JobOutcome, OtherGuestConfirmation, PlanEnter, PlanEntry, PlanItem,
    PlanLeave, RestorePlanBatch, RestoreSpec, RestoreTotals,
};
use proptest::prelude::*;
use proptest::test_runner::TestRunner;
use prost::Message;
use rustix::fs::{FileType, Mode, OFlags};
use serde_json::json;
use tokio_util::sync::CancellationToken;

pub(crate) const NODE: &str = "pve1";
pub(crate) const UID_X: &str = "abcdefghijklmnopqrst";
pub(crate) const UID_Y: &str = "bbbbbbbbbbbbbbbbbbbb";
pub(crate) const UID_Z: &str = "cccccccccccccccccccc";
const UID_V: &str = "dddddddddddddddddddd";
const UID_W: &str = "eeeeeeeeeeeeeeeeeeee";
pub(crate) const UUID: &str = "0192f0c4-7d3a-7000-8000-000000000001";
const UUID_NEW: &str = "0192f0c4-7d3a-7000-8000-000000000002";
const WAIT: Duration = Duration::from_secs(120);
const PAUSE: Duration = Duration::from_millis(10);
pub(crate) const TARGET_MAC: &str = "BC:24:11:00:00:02";
const ROOTFS: &str = "local-lvm:vm-200-disk-0,size=8G";

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
        self.pct_script_with(fail, false);
    }

    /// With `gate`, `pct` waits for `pct-go` after it created `pct-started`,
    /// so a test can look at the staging folder as `pct` finds it.
    pub fn pct_script_with(&self, fail: Option<&str>, gate: bool) {
        let proxmox = proxmox_dir();
        let wait = if gate {
            format!(
                "touch '{started}'\nwhile [ ! -e '{proceed}' ]; do sleep 0.05; done\n",
                started = self.path("pct-started").display(),
                proceed = self.path("pct-go").display(),
            )
        } else {
            String::new()
        };
        let body = match fail {
            Some(text) => format!("echo '{text}' >&2\nexit 1\n"),
            None => format!(
                "{wait}LODESTORE_FAKE_PCT='{scenario}' exec perl -t -I '{stubs}' -I '{lib}' '{fake}' \"$@\"\n",
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

    pub fn allow_replace(&self) {
        self.agent_yaml("restore:\n  allow_guest_in_place: true\n");
    }

    pub fn staging_default(&self) -> PathBuf {
        self.agent.paths.state_dir.join("pve-staging")
    }

    /// CT 200 on this node with `uid` and the saved instance `uuid`.
    pub fn target(&self, uid: Option<&str>, uuid: &str, extra: &str) {
        let meta = uid.map_or(String::new(), |u| {
            format!("meta: ctime=1700000000,uid={u},rtime=5\n")
        });
        write(
            &self.pve.join(format!("nodes/{NODE}/lxc/200.conf")),
            &format!(
                "arch: amd64\nhostname: ct200\n{meta}net0: name=eth0,bridge=vmbr0,hwaddr={TARGET_MAC},firewall=1,type=veth\nrootfs: {ROOTFS}\nunprivileged: 1\n{extra}"
            ),
        );
        write(
            &self.agent.paths.source_ids(),
            &json!({"ct/200": {"uuid": uuid, "pve_uid": uid.unwrap_or_default()}}).to_string(),
        );
        self.vmlist(&[200]);
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

    /// Runs the restore and calls `inspect` while a gated `pct` waits.
    pub async fn run_gated(
        &self,
        snap: &Snap,
        spec: RestoreSpec,
        inspect: impl FnOnce() + Send + 'static,
    ) -> Ran {
        let (started, proceed) = (self.path("pct-started"), self.path("pct-go"));
        for f in [&started, &proceed] {
            let _ = fs::remove_file(f);
        }
        let gate = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + WAIT;
            while !started.exists() {
                assert!(std::time::Instant::now() < deadline, "pct did not start");
                std::thread::sleep(PAUSE);
            }
            inspect();
            fs::write(proceed, "").unwrap();
        });
        let ran = self.run(snap, spec).await;
        gate.join().unwrap();
        ran
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

pub(crate) fn expected(uid: &str, uuid: &str) -> ExpectedGuest {
    ExpectedGuest {
        vmid: 200,
        name: "ct200".into(),
        pve_uid: uid.into(),
        pve_rtime: 5,
        instance_uuid: uuid.into(),
        volumes: vec![ConfigLine {
            key: "rootfs".into(),
            value: ROOTFS.into(),
        }],
    }
}

pub(crate) fn replace(uid: &str, confirm: Option<(&str, &str)>) -> GuestTarget {
    GuestTarget {
        node: NODE.into(),
        vmid: 200,
        in_place: true,
        storage: "local-lvm".into(),
        expected: Some(expected(uid, UUID)),
        confirm_other_guest: confirm.map(|(s, t)| OtherGuestConfirmation {
            snapshot_pve_uid: s.into(),
            target_pve_uid: t.into(),
        }),
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

#[tokio::test(flavor = "multi_thread")]
async fn ct_restore_replace_needs_local_key() {
    let lab = Lab::new().await;
    lab.target(Some(UID_X), UUID, "");
    let snap = lab
        .snapshot(&snapshot_config(Some(UID_X), ""), None, |_| {}, &[])
        .await;
    let ran = lab.run(&snap, lab.spec(&snap, replace(UID_X, None))).await;
    failed_with(&ran, "guest_in_place_disabled");
    assert!(ran.argv.is_none());
    assert!(!lab.staging_default().exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_restore_replace_running_refused() {
    let lab = Lab::new().await;
    lab.allow_replace();
    lab.target(Some(UID_X), UUID, "");
    fs::create_dir_all(lab.sys.join("fs/cgroup/lxc/200")).unwrap();
    let snap = lab
        .snapshot(&snapshot_config(Some(UID_X), ""), None, |_| {}, &[])
        .await;
    let ran = lab.run(&snap, lab.spec(&snap, replace(UID_X, None))).await;
    failed_with(&ran, "guest_running");
    assert_eq!(
        ran.done.error_message,
        "CT 200 · ct200 is running. Stop it in Proxmox, then try again."
    );
    assert!(ran.argv.is_none());
    assert!(!lab.staging_default().exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_restore_replace_ha_refused() {
    let lab = Lab::new().await;
    lab.allow_replace();
    lab.target(Some(UID_X), UUID, "");
    write(
        &lab.pve.join("ha/resources.cfg"),
        "ct: 200\n\tstate started\n",
    );
    let snap = lab
        .snapshot(&snapshot_config(Some(UID_X), ""), None, |_| {}, &[])
        .await;
    let ran = lab.run(&snap, lab.spec(&snap, replace(UID_X, None))).await;
    failed_with(&ran, "guest_ha_managed");
    assert_eq!(
        ran.done.error_message,
        "CT 200 · ct200 is managed by HA. Remove it from HA in Proxmox, then try again."
    );
    assert!(ran.argv.is_none());
    assert!(!lab.staging_default().exists());
}

fn replace_argv(extra: &[&str]) -> Vec<String> {
    let mut argv = new_argv(200, extra);
    argv[3] = "--force".into();
    argv
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_restore_replace_argv_and_macs() {
    let lab = Lab::new().await;
    lab.allow_replace();
    lab.target(Some(UID_X), UUID, "");
    let snap = lab
        .snapshot(&snapshot_config(Some(UID_X), ""), None, |_| {}, &[])
        .await;
    let ran = lab.run(&snap, lab.spec(&snap, replace(UID_X, None))).await;
    succeeded(&ran);
    let argv = argv_shape(&ran);
    assert_eq!(argv, replace_argv(&[]));
    assert!(
        !argv
            .iter()
            .any(|a| a == "--unique" || a == "--allow-identity-mismatch")
    );
    let config = ran.config.as_deref().unwrap();
    let net0 = config.lines().find(|l| l.starts_with("net0:")).unwrap();
    assert!(net0.contains(&format!("hwaddr={TARGET_MAC}")), "{net0}");
    assert!(net0.contains("firewall=1"), "{net0}");
    let guest = ran.guest();
    assert!(guest.replaced);
    assert!(guest.new_mac_nets.is_empty(), "{guest:?}");
    assert!(lab.staging_clean(&lab.staging_default()));
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_restore_replace_target_recreated() {
    let lab = Lab::new().await;
    lab.allow_replace();
    lab.target(Some(UID_Z), UUID_NEW, "");
    let snap = lab
        .snapshot(&snapshot_config(Some(UID_X), ""), None, |_| {}, &[])
        .await;
    let ran = lab.run(&snap, lab.spec(&snap, replace(UID_X, None))).await;
    failed_with(&ran, "guest_target_changed");
    assert!(ran.argv.is_none());
    assert!(lab.staging_clean(&lab.staging_default()));
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_restore_replace_no_backup_uid_target_changed() {
    let lab = Lab::new().await;
    lab.allow_replace();
    let snap = lab
        .snapshot(&snapshot_config(None, ""), None, |_| {}, &[])
        .await;
    lab.target(Some(UID_Z), UUID_NEW, "");
    let spec = lab.spec(&snap, replace(UID_X, Some(("", UID_X))));
    let ran = lab.run(&snap, spec.clone()).await;
    failed_with(&ran, "guest_target_changed");
    assert!(ran.argv.is_none());

    lab.target(Some(UID_X), UUID, "");
    let ran = lab.run(&snap, spec).await;
    succeeded(&ran);
    assert_eq!(argv_shape(&ran), replace_argv(&[]));
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_restore_replace_volumes_changed() {
    let lab = Lab::new().await;
    lab.allow_replace();
    lab.target(
        Some(UID_X),
        UUID,
        "mp1: local-lvm:vm-200-disk-1,mp=/data,backup=0,size=1G\n",
    );
    let snap = lab
        .snapshot(&snapshot_config(Some(UID_X), ""), None, |_| {}, &[])
        .await;
    let ran = lab.run(&snap, lab.spec(&snap, replace(UID_X, None))).await;
    failed_with(&ran, "guest_target_changed");
    assert!(ran.done.error_message.contains("volumes"), "{:?}", ran.done);
    assert!(ran.argv.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_restore_identity_mismatch() {
    let lab = Lab::new().await;
    lab.allow_replace();
    let snap = lab
        .snapshot(&snapshot_config(Some(UID_X), ""), None, |_| {}, &[])
        .await;
    for (confirm, ok) in [
        (None, false),
        (Some((UID_X, UID_W)), false),
        (Some((UID_V, UID_Y)), false),
        (Some((UID_X, UID_Y)), true),
    ] {
        lab.target(Some(UID_Y), UUID, "");
        let ran = lab
            .run(&snap, lab.spec(&snap, replace(UID_Y, confirm)))
            .await;
        if ok {
            succeeded(&ran);
            assert_eq!(
                argv_shape(&ran),
                replace_argv(&["--allow-identity-mismatch", "1"])
            );
        } else {
            failed_with(&ran, "guest_identity_mismatch");
            assert!(ran.argv.is_none(), "{confirm:?}");
        }
        assert!(lab.staging_clean(&lab.staging_default()));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_restore_target_missing() {
    let lab = Lab::new().await;
    lab.allow_replace();
    let snap = lab
        .snapshot(&snapshot_config(Some(UID_X), ""), None, |_| {}, &[])
        .await;
    let ran = lab.run(&snap, lab.spec(&snap, replace(UID_X, None))).await;
    failed_with(&ran, "guest_not_found");
    assert!(ran.argv.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_restore_vmid_taken_and_storage() {
    let lab = Lab::new().await;
    lab.target(Some(UID_X), UUID, "");
    let snap = lab
        .snapshot(&snapshot_config(None, ""), None, |_| {}, &[])
        .await;
    let ran = lab.run(&snap, lab.spec(&snap, new_ct(200))).await;
    failed_with(&ran, "restore_vmid_taken");
    let local = GuestTarget {
        storage: "local".into(),
        ..new_ct(105)
    };
    let ran = lab.run(&snap, lab.spec(&snap, local)).await;
    failed_with(&ran, "restore_storage_invalid");
    let other = GuestTarget {
        node: "pve2".into(),
        ..new_ct(105)
    };
    let ran = lab.run(&snap, lab.spec(&snap, other)).await;
    failed_with(&ran, "guest_not_local");
    assert_eq!(ran.done.error_detail["vmid"], "105");
    assert!(ran.argv.is_none());
    assert!(!lab.staging_default().exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_restore_no_space() {
    let lab = Lab::new().await;
    let snap = lab
        .snapshot(&snapshot_config(None, ""), None, |_| {}, &[])
        .await;
    let reserve = SpaceReserve {
        min_bytes: u64::MAX / 2,
        percent: 0,
    };
    let ran = lab
        .run_with(&snap, lab.spec(&snap, new_ct(105)), reserve)
        .await;
    failed_with(&ran, "restore_no_space");
    assert!(ran.argv.is_none());
    assert!(lab.staging_clean(&lab.staging_default()));
}

#[tokio::test(flavor = "multi_thread")]
async fn ct_restore_pct_fails_cleans_staging() {
    let lab = Lab::new().await;
    lab.pct_script(Some("unable to restore CT 105 - boom"));
    let snap = lab
        .snapshot(&snapshot_config(None, ""), None, |_| {}, &[])
        .await;
    let ran = lab.run(&snap, lab.spec(&snap, new_ct(105))).await;
    failed_with(&ran, "pct_restore_failed");
    assert!(ran.done.error_message.contains("boom"), "{:?}", ran.done);
    assert!(lab.staging_clean(&lab.staging_default()));

    let base = lab.staging_default();
    let stale = base.join(format!("lodestore-restore-{}", "ab".repeat(16)));
    write(&stale.join("data/fs/etc/hostname"), "x\n");
    fs::create_dir_all(base.join("not-a-restore")).unwrap();
    let unrelated = base.join("ef".repeat(16));
    write(&unrelated.join("keep"), "x\n");
    let held = base.join(format!("lodestore-restore-{}", "cd".repeat(16)));
    fs::create_dir_all(&held).unwrap();
    let lock = rustix::fs::open(
        held.join(".lock"),
        OFlags::CREATE | OFlags::RDWR,
        Mode::RUSR | Mode::WUSR,
    )
    .unwrap();
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive).unwrap();
    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    drop(lock);

    clean_stale_staging(&base);
    assert!(!stale.exists());
    assert!(base.join("not-a-restore").exists());
    assert!(
        unrelated.join("keep").exists(),
        "a folder without the prefix is never removed"
    );
    assert!(
        held.exists(),
        "a folder whose lock a live process holds stays"
    );

    child.kill().unwrap();
    child.wait().unwrap();
    clean_stale_staging(&base);
    assert!(!held.exists());
}

/// Changes the plan entry named `name`.
fn edit_entry(snap: &mut Snap, name: &[u8], edit: impl Fn(&mut Node, &mut Vec<ChunkLoc>)) {
    let mut found = false;
    for item in &mut snap.batches[0].items {
        if let Some(Kind::Entry(e)) = &mut item.kind {
            let mut node = Node::decode(e.node.as_slice()).unwrap();
            if node.name == name {
                edit(&mut node, &mut e.chunks);
                e.node = node.encode_to_vec();
                found = true;
            }
        }
    }
    assert!(found, "no entry {}", String::from_utf8_lossy(name));
}

/// An entry that failed to restore leaves the staging copy incomplete:
/// neither a new container nor a replace is started from it.
#[tokio::test(flavor = "multi_thread")]
async fn ct_restore_incomplete_staging_refused() {
    let lab = Lab::new().await;
    lab.allow_replace();
    let bad_name = Node {
        name: b"a/b".to_vec(),
        r#type: NodeType::File as i32,
        mode: 0o644,
        ..Default::default()
    };
    let corrupt = |snap: &mut Snap| {
        edit_entry(snap, b"hostname", |_, chunks| chunks[0].id[0] ^= 1);
    };
    for replace_it in [false, true] {
        let (uid, guest) = if replace_it {
            lab.target(Some(UID_X), UUID, "");
            (Some(UID_X), replace(UID_X, None))
        } else {
            (None, new_ct(105))
        };
        let config = snapshot_config(uid, "");
        let mut corrupted = lab.snapshot(&config, None, |_| {}, &[]).await;
        corrupt(&mut corrupted);
        let rejected = lab
            .snapshot(&config, None, |_| {}, &[("etc", bad_name.clone())])
            .await;
        let no_etc = lab
            .snapshot(
                &config,
                None,
                |root| fs::remove_dir_all(root.join("etc")).unwrap(),
                &[],
            )
            .await;
        for snap in [&corrupted, &rejected, &no_etc] {
            let ran = lab.run(snap, lab.spec(snap, guest.clone())).await;
            failed_with(&ran, "restore_incomplete");
            assert!(ran.argv.is_none(), "{:?}", ran.done);
            assert!(lab.staging_clean(&lab.staging_default()));
        }
    }
}

/// `pct restore --force` destroys the target's snapshots and the volumes
/// only they use, which the confirmation does not show.
#[tokio::test(flavor = "multi_thread")]
async fn ct_restore_replace_target_with_snapshots_refused() {
    let lab = Lab::new().await;
    lab.allow_replace();
    let snap = lab
        .snapshot(&snapshot_config(Some(UID_X), ""), None, |_| {}, &[])
        .await;
    lab.target(
        Some(UID_X),
        UUID,
        "\n[snap1]\narch: amd64\nhostname: ct200\nrootfs: local-lvm:vm-200-disk-0,size=8G\nsnaptime: 1700000001\n",
    );
    let ran = lab.run(&snap, lab.spec(&snap, replace(UID_X, None))).await;
    failed_with(&ran, "guest_target_has_snapshots");
    assert!(ran.argv.is_none());
    assert!(!lab.staging_default().exists());

    lab.target(Some(UID_X), UUID, "\n[pve:pending]\nmemory: 1024\n");
    let ran = lab.run(&snap, lab.spec(&snap, replace(UID_X, None))).await;
    succeeded(&ran);
}

/// A `.lock` in the plan lands outside the staging folder's lock: the
/// folder stays held while `pct` runs.
#[tokio::test(flavor = "multi_thread")]
async fn ct_restore_planted_lock_keeps_staging_held() {
    let lab = Lab::new().await;
    lab.pct_script_with(None, true);
    let mut snap = lab
        .snapshot(&snapshot_config(None, ""), None, |_| {}, &[])
        .await;
    let lock = Node {
        name: b".lock".to_vec(),
        r#type: NodeType::Fifo as i32,
        mode: 0o600,
        ..Default::default()
    };
    snap.batches[0].items.insert(
        0,
        PlanItem {
            kind: Some(Kind::Entry(PlanEntry {
                node: lock.encode_to_vec(),
                chunks: Vec::new(),
                more: false,
            })),
        },
    );
    let base = lab.staging_default();
    let (tx, rx) = std::sync::mpsc::channel();
    let inspect = move || {
        let dirs: Vec<PathBuf> = fs::read_dir(&base)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        clean_stale_staging(&base);
        tx.send(dirs.iter().all(|d| d.exists())).unwrap();
    };
    let ran = lab
        .run_gated(&snap, lab.spec(&snap, new_ct(105)), inspect)
        .await;
    assert!(
        rx.recv().unwrap(),
        "the staging folder was removed while pct ran"
    );
    succeeded(&ran);
}

/// A FIFO as `pct.conf` is refused instead of blocking the agent.
#[tokio::test(flavor = "multi_thread")]
async fn ct_restore_config_fifo_refused() {
    let lab = Lab::new().await;
    let mut snap = lab
        .snapshot(&snapshot_config(None, ""), None, |_| {}, &[])
        .await;
    edit_entry(&mut snap, b"pct.conf", |node, chunks| {
        node.r#type = NodeType::Fifo as i32;
        node.size = 0;
        chunks.clear();
    });
    let ran = lab.run(&snap, lab.spec(&snap, new_ct(105))).await;
    failed_with(&ran, "restore_config_invalid");
    assert!(ran.argv.is_none());
    assert!(lab.staging_clean(&lab.staging_default()));
}

#[derive(Debug, Clone)]
enum Planted {
    File(Vec<u8>, bool),
    Dir,
    Symlink(usize),
    Fifo,
    Device(bool),
}

#[derive(Debug, Clone)]
struct Entry {
    parent: usize,
    kind: Planted,
    uid: u32,
    gid: u32,
    mode: u32,
    xattr: Option<u8>,
    acl: Option<u32>,
}

const PARENTS: [&str; 3] = ["", "d0", "d0/d1"];

fn planted_entry() -> impl Strategy<Value = Entry> {
    let kind = prop_oneof![
        4 => (prop::collection::vec(any::<u8>(), 0..2048), any::<bool>())
            .prop_map(|(data, setuid)| Planted::File(data, setuid)),
        2 => Just(Planted::Dir),
        1 => (0usize..8).prop_map(Planted::Symlink),
        1 => Just(Planted::Fifo),
        1 => any::<bool>().prop_map(Planted::Device),
    ];
    (
        0..PARENTS.len(),
        kind,
        0u32..70_000,
        0u32..70_000,
        prop::sample::select(&[0o600u32, 0o640, 0o644, 0o700, 0o750, 0o755][..]),
        proptest::option::of(any::<u8>()),
        proptest::option::of(1u32..70_000),
    )
        .prop_map(|(parent, kind, uid, gid, mode, xattr, acl)| Entry {
            parent,
            kind,
            uid,
            gid,
            mode,
            xattr,
            acl,
        })
}

/// Builds the planted tree under `root`; returns how many device nodes it has.
fn plant(root: &Path, entries: &[Entry]) -> u64 {
    use std::os::unix::fs::symlink;
    for parent in PARENTS.iter().skip(1) {
        fs::create_dir_all(root.join(parent)).unwrap();
    }
    let mut devices = 0;
    for (i, e) in entries.iter().enumerate() {
        let path = root.join(PARENTS[e.parent]).join(format!("e{i}"));
        let node = |ty, dev| {
            rustix::fs::mknodat(rustix::fs::CWD, &path, ty, Mode::from_raw_mode(e.mode), dev)
                .unwrap();
        };
        match &e.kind {
            Planted::File(data, _) => fs::write(&path, data).unwrap(),
            Planted::Dir => fs::create_dir(&path).unwrap(),
            Planted::Symlink(t) => symlink(format!("e{t}"), &path).unwrap(),
            Planted::Fifo => node(FileType::Fifo, 0),
            Planted::Device(block) => {
                devices += 1;
                if *block {
                    node(FileType::BlockDevice, rustix::fs::makedev(8, 0));
                } else {
                    node(FileType::CharacterDevice, rustix::fs::makedev(1, 3));
                }
            }
        }
        rustix::fs::chownat(
            rustix::fs::CWD,
            &path,
            Some(rustix::process::Uid::from_raw(e.uid)),
            Some(rustix::process::Gid::from_raw(e.gid)),
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        )
        .unwrap();
        if matches!(e.kind, Planted::Symlink(_)) {
            continue;
        }
        let setuid = matches!(e.kind, Planted::File(_, true));
        let mode = e.mode | if setuid { 0o4000 } else { 0 };
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        if !matches!(e.kind, Planted::File(..) | Planted::Dir) {
            continue;
        }
        if let Some(x) = e.xattr {
            rustix::fs::setxattr(
                &path,
                format!("user.k{x}").as_str(),
                format!("v{x}").as_bytes(),
                rustix::fs::XattrFlags::empty(),
            )
            .unwrap();
        }
        if let Some(uid) = e.acl {
            let out = std::process::Command::new("setfacl")
                .arg("-m")
                .arg(format!("u:{uid}:r-x"))
                .arg(&path)
                .output()
                .expect("setfacl is needed for this test");
            assert!(out.status.success(), "{out:?}");
        }
    }
    devices
}

#[derive(Debug, PartialEq)]
struct Desc {
    kind: u32,
    mode: u32,
    uid: u32,
    gid: u32,
    xattrs: BTreeMap<Vec<u8>, Vec<u8>>,
    content: Vec<u8>,
}

/// Every entry below `root` with type, mode, owners, xattrs (ACLs among
/// them) and content; device nodes are left out.
fn describe(root: &Path) -> BTreeMap<String, Desc> {
    use std::os::unix::fs::MetadataExt;
    let mut out = BTreeMap::new();
    let mut todo = vec![PathBuf::new()];
    while let Some(rel) = todo.pop() {
        let path = root.join(&rel);
        let meta = fs::symlink_metadata(&path).unwrap();
        let ft = meta.file_type();
        if ft.is_block_device() || ft.is_char_device() {
            continue;
        }
        let mut xattrs = BTreeMap::new();
        if !ft.is_symlink() {
            let mut names = vec![0u8; 4096];
            let n = rustix::fs::listxattr(&path, &mut names).unwrap();
            for name in names[..n].split(|b| *b == 0).filter(|n| !n.is_empty()) {
                let mut value = vec![0u8; 4096];
                let c = std::ffi::CString::new(name).unwrap();
                let len = rustix::fs::getxattr(&path, c.as_c_str(), &mut value).unwrap();
                xattrs.insert(name.to_vec(), value[..len].to_vec());
            }
        }
        let content = if ft.is_file() {
            fs::read(&path).unwrap()
        } else if ft.is_symlink() {
            fs::read_link(&path)
                .unwrap()
                .into_os_string()
                .into_encoded_bytes()
        } else {
            Vec::new()
        };
        if ft.is_dir() {
            for entry in fs::read_dir(&path).unwrap() {
                todo.push(rel.join(entry.unwrap().file_name()));
            }
        }
        let mode = if ft.is_symlink() {
            0
        } else {
            meta.mode() & 0o7777
        };
        out.insert(
            rel.display().to_string(),
            Desc {
                kind: meta.mode() & 0o170000,
                mode,
                uid: meta.uid(),
                gid: meta.gid(),
                xattrs,
                content,
            },
        );
    }
    out
}

/// The staging folder `pct` gets is the snapshot's `/fs` as the container
/// sees it: types, modes with setuid, owners, xattrs, ACLs and content; no
/// device nodes, and each one counted.
#[test]
#[ignore = "needs root"]
fn pve_root_staging_matches_snapshot() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let lab = rt.block_on(Lab::new());
    lab.pct_script_with(None, true);
    let mut runner = TestRunner::new(ProptestConfig {
        cases: 16,
        failure_persistence: None,
        ..ProptestConfig::default()
    });
    let tree = prop::collection::vec(planted_entry(), 0..12);
    runner
        .run(&tree, |entries| {
            let _ = fs::remove_file(lab.pve.join(format!("nodes/{NODE}/lxc/105.conf")));
            let source = Arc::new(std::sync::Mutex::new(None));
            let devices = Arc::new(AtomicUsize::new(0));
            let (src, dev) = (Arc::clone(&source), Arc::clone(&devices));
            let config = snapshot_config(None, "");
            let planted = move |root: &Path| {
                dev.store(plant(root, &entries) as usize, Ordering::Relaxed);
                *src.lock().unwrap() = Some(describe(root));
            };
            let snap = rt.block_on(lab.snapshot(&config, None, planted, &[]));
            let expected = source.lock().unwrap().take().unwrap();
            let base = lab.staging_default();
            let (tx, rx) = std::sync::mpsc::channel();
            let inspect = move || {
                let mut dirs = fs::read_dir(&base).unwrap().map(|e| e.unwrap().path());
                let staging = dirs.next().unwrap();
                assert!(dirs.next().is_none());
                tx.send(describe(&staging.join("data/fs"))).unwrap();
            };
            let spec = lab.spec(&snap, new_ct(105));
            let ran = rt.block_on(lab.run_gated(&snap, spec, inspect));
            prop_assert!(ran.done.error_code.is_empty(), "{:?}", ran.msgs);
            let staged = rx.recv().unwrap();
            prop_assert_eq!(&staged, &expected);
            prop_assert_eq!(
                ran.done.restore.as_ref().unwrap().special_skipped,
                devices.load(Ordering::Relaxed) as u64
            );
            prop_assert!(lab.staging_clean(&lab.staging_default()));
            Ok(())
        })
        .unwrap();
}
