use crate::common;

use std::collections::BTreeSet;
use std::os::fd::OwnedFd;

use common::fake_agent::{Fake, Host, is_ack};
use common::{Api, TestServer, backends};
use lodestore_agent::{EnrollArgs, NetOptions};
use lodestore_format::ct::{CtMeta, CtMode, IdMap};
use lodestore_format::guest_config::{FilterInput, filter_ct};
use lodestore_format::index::hex32;
use lodestore_format::repo::Repository;
use lodestore_format::source::{Meta, SourceKey};
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, CtBackup, Writer};
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::restore_spec::Target;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{
    ClusterNode, ConfigLine, DroppedConfigKey, GuestKind, GuestRestoreResult, GuestTarget,
    JobAccepted, JobFinished, JobOutcome, ProxmoxFacts, ProxmoxGuest, ProxmoxStorage, RestoreSpec,
    RestoreStats, SourceFacts,
};
use lodestore_server::catalog::ingest_snapshot;
use lodestore_server::db::now_ms;
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;
use serde_json::{Value, json};
use sqlx::Row;

const PASSWORD: &[u8] = b"test-password";
const CLUSTER: &str = "pve-prod";
const CAPS: &str = r#"["restore_v1","restore_roots_v1","guest_restore_ct_v1","source:proxmox-ct"]"#;
const AGENT_CAPS: [&str; 4] = [
    "restore_v1",
    "restore_roots_v1",
    "guest_restore_ct_v1",
    "source:proxmox-ct",
];
pub(crate) const CONFIG: &str = "arch: amd64\nhostname: dns\nrootfs: local-lvm:vm-200-disk-0,size=8G\n\
                      net0: name=eth0,bridge=vmbr0,hwaddr=AA:BB:CC:00:00:01\nunprivileged: 1\n";

pub(crate) async fn enroll(ts: &TestServer, api: &Api, repo: &str, hostname: &str) -> Host {
    let token = api.join_token(repo, json!([])).await;
    let paths = ts.agent_paths(hostname);
    let enrolled = lodestore_agent::enroll(EnrollArgs {
        token,
        paths: paths.clone(),
        hostname: Some(hostname.to_string()),
        net: NetOptions::default(),
    })
    .await
    .expect("enroll");
    Host {
        id: enrolled.agent_id,
        paths,
    }
}

pub(crate) async fn set_capabilities(ts: &TestServer, host: &Host, capabilities: &str) {
    sqlx::query("UPDATE agents SET capabilities_json = $1 WHERE id = $2")
        .bind(capabilities)
        .bind(&host.id)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
}

fn uuid_of(vmid: u32) -> String {
    format!("{vmid:08}-2222-4222-8222-222222222222")
}

pub(crate) fn ct(vmid: u32) -> ProxmoxGuest {
    ProxmoxGuest {
        kind: GuestKind::Ct as i32,
        vmid,
        name: format!("ct-{vmid}"),
        uuid: uuid_of(vmid),
        ..Default::default()
    }
}

pub(crate) fn node_facts(cluster: &str, node: &str, guests: Vec<ProxmoxGuest>) -> ProxmoxFacts {
    ProxmoxFacts {
        cluster: cluster.to_string(),
        node: node.to_string(),
        guests,
        storages: vec![
            ProxmoxStorage {
                id: "local-lvm".into(),
                r#type: "lvmthin".into(),
                content: vec!["rootdir".into(), "images".into()],
                shared: false,
            },
            ProxmoxStorage {
                id: "local".into(),
                r#type: "dir".into(),
                content: vec!["iso".into(), "backup".into()],
                shared: false,
            },
        ],
        plugin_installed: true,
        next_vmid: 105,
        bridges: vec!["vmbr0".into()],
        ..Default::default()
    }
}

pub(crate) async fn seed_facts(ts: &TestServer, host: &Host, facts: ProxmoxFacts, age_ms: u64) {
    let facts = SourceFacts {
        age_ms,
        proxmox: Some(facts),
        ..Default::default()
    };
    lodestore_server::sources::record(ts.state(), &host.id, &facts, &mut None, now_ms())
        .await
        .unwrap();
}

/// A signed container snapshot of CT `vmid` · dns with `config` as `pct.conf`.
pub(crate) async fn write_ct(
    ts: &TestServer,
    repo_id: &str,
    host: &Host,
    vmid: u32,
    config: &str,
    firewall: Option<&str>,
) -> String {
    let idmap = IdMap::default_unprivileged();
    write_ct_mapped(ts, repo_id, host, (vmid, config, firewall), idmap).await
}

pub(crate) async fn write_ct_mapped(
    ts: &TestServer,
    repo_id: &str,
    host: &Host,
    (vmid, config, firewall): (u32, &str, Option<&str>),
    idmap: IdMap,
) -> String {
    let store = LocalFsStore::new(ts.local_repo_root("main"));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    let mut writer = Writer::new(&store, keys, repo.config()).await.unwrap();
    let root = tempfile::tempdir_in(ts.dir.path()).unwrap();
    std::fs::create_dir_all(root.path().join("etc")).unwrap();
    std::fs::write(root.path().join("etc/hostname"), b"dns\n").unwrap();
    let dir = OwnedFd::from(std::fs::File::open(root.path()).unwrap());
    let backup = CtBackup {
        key: SourceKey::proxmox_ct(CLUSTER, &uuid_of(vmid)).unwrap(),
        meta: CtMeta {
            mode: CtMode::Snapshot,
            unprivileged: true,
            idmap: Some(idmap),
        },
        source_meta: Meta::from([
            ("vmid".to_owned(), vmid.to_string()),
            ("name".to_owned(), "dns".to_owned()),
        ]),
        config: config.to_owned(),
        firewall: firewall.map(str::to_owned),
        exclude: None,
    };
    let options = BackupOptions {
        hostname: Some(b"pve1".to_vec()),
        agent_id: Some(host.id.as_bytes().to_vec()),
        ..BackupOptions::default()
    };
    let pending = writer
        .backup_container(&dir, backup, options, &|_| {})
        .await
        .expect("container backup");
    let outcome = pending.commit(&mut writer).await.unwrap();
    let snapshot = hex32(&outcome.snapshot_id);
    ingest_snapshot(ts.state(), repo_id, &snapshot, None)
        .await
        .expect("ingest");
    sqlx::query(
        "UPDATE snapshots SET verified_at = $1, verified_by = 'owner', \
         source_type = 'proxmox-ct', source_ref = $2 WHERE id = $3",
    )
    .bind(now_ms())
    .bind(format!("proxmox-ct/{CLUSTER}/{}", uuid_of(vmid)))
    .bind(&snapshot)
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
    snapshot
}

pub(crate) async fn job_count(ts: &TestServer) -> i64 {
    sqlx::query("SELECT COUNT(*) AS n FROM jobs WHERE kind = 'restore'")
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .get("n")
}

pub(crate) async fn post(api: &Api, body: &Value) -> (u16, Value) {
    let response = api.post("/api/v1/restores/guest", body).await;
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

fn new_body(snapshot: &str, host: &Host) -> Value {
    json!({
        "snapshot_id": snapshot,
        "target_host_id": host.id,
        "storage": "local-lvm",
        "vmid": 105,
    })
}

fn assigned_restore(body: ServerBody) -> Option<(String, RestoreSpec)> {
    match body {
        ServerBody::JobAssign(a) => a.restore.map(|r| (a.job_id, r)),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_restore_new_ct_roundtrip() {
    for backend in backends() {
        let name = backend.name();
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "pve1").await;
        set_capabilities(&ts, &host, CAPS).await;
        seed_facts(&ts, &host, node_facts(CLUSTER, "pve1", vec![ct(200)]), 0).await;
        let snapshot = write_ct(&ts, &repo, &host, 200, CONFIG, None).await;

        let (status, body) = post(&api, &new_body(&snapshot, &host)).await;
        assert_eq!(status, 201, "[{name}] {body}");
        let job_id = body["job_id"].as_str().unwrap().to_string();

        let mut agent = Fake::connect_with(&ts, &host, &AGENT_CAPS).await;
        let (assigned, spec) = agent.recv(assigned_restore).await;
        assert_eq!(assigned, job_id, "[{name}]");
        let Some(Target::Guest(target)) = spec.target.clone() else {
            panic!("[{name}] not a guest target: {spec:?}");
        };
        assert_eq!(
            target,
            GuestTarget {
                node: "pve1".into(),
                vmid: 105,
                in_place: false,
                storage: "local-lvm".into(),
                ..Default::default()
            },
            "[{name}]"
        );
        assert!(spec.destination.is_empty() && !spec.in_place, "[{name}]");
        assert_eq!(spec.snapshot_id, snapshot, "[{name}]");
        let record: Value = serde_json::from_str(
            &sqlx::query("SELECT restore_json FROM jobs WHERE id = $1")
                .bind(&job_id)
                .fetch_one(&ts.state().db.pool)
                .await
                .unwrap()
                .get::<String, _>("restore_json"),
        )
        .unwrap();
        assert_eq!(
            record["items_b64"],
            json!(["L2Zz", "L2NvbmZpZw"]),
            "[{name}]"
        );

        agent
            .send(AgentBody::JobAccepted(JobAccepted {
                job_id: job_id.clone(),
                started_at_ms: now_ms(),
            }))
            .await;
        let dropped = (0..300)
            .map(|i| DroppedConfigKey {
                key: format!("lxc.key{i}"),
                value: "v".repeat(1024),
                reason: if i == 0 { "sneaky" } else { "lxc" }.into(),
            })
            .collect();
        agent
            .send(AgentBody::JobFinished(JobFinished {
                job_id: job_id.clone(),
                outcome: JobOutcome::Succeeded as i32,
                finished_at_ms: now_ms(),
                restore: Some(RestoreStats {
                    files: 2,
                    guest: Some(GuestRestoreResult {
                        vmid: 105,
                        node: "pve1".into(),
                        storage: "local-lvm".into(),
                        name: "dns".into(),
                        dropped,
                        new_mac_nets: vec!["net0".into()],
                        unprivileged: true,
                        root_host_uid: 100000,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }))
            .await;
        assert_eq!(agent.recv(is_ack).await, job_id, "[{name}]");

        let job: Value = api
            .get(&format!("/api/v1/jobs/{job_id}"))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(job["state"], "succeeded", "[{name}] {job}");
        assert_eq!(job["restore"]["guest"]["vmid"], 105, "[{name}]");
        assert_eq!(job["restore"]["guest"]["replace"], false, "[{name}]");
        let guest = &job["restore_result"]["guest"];
        let dropped = guest["dropped"].as_array().unwrap();
        assert_eq!(dropped.len(), 256, "[{name}]");
        assert_eq!(guest["dropped_total"], 300, "[{name}]");
        assert!(
            dropped
                .iter()
                .all(|d| d["value"].as_str().unwrap().len() <= 256),
            "[{name}]"
        );
        assert_eq!(dropped[0]["reason"], "unknown", "[{name}]");
        assert_eq!(dropped[1]["reason"], "lxc", "[{name}]");
        assert_eq!(guest["root_host_uid"], 100000, "[{name}]");
        assert_eq!(guest["new_mac_nets"], json!(["net0"]), "[{name}]");
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_restore_agent_outdated() {
    for backend in backends() {
        let name = backend.name();
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "pve1").await;
        set_capabilities(&ts, &host, r#"["restore_v1","restore_roots_v1"]"#).await;
        seed_facts(&ts, &host, node_facts(CLUSTER, "pve1", vec![]), 0).await;
        let snapshot = write_ct(&ts, &repo, &host, 200, CONFIG, None).await;

        let (status, body) = post(&api, &new_body(&snapshot, &host)).await;
        assert_eq!(status, 422, "[{name}] {body}");
        assert_eq!(body["code"], "restore_agent_outdated", "[{name}]");
        assert_eq!(job_count(&ts).await, 0, "[{name}]");
        ts.stop().await;
    }
}

const LINES: &[&str] = &[
    "arch: amd64",
    "hostname: web",
    "hostname: db\r",
    "cores: 4",
    "# description",
    "description: text",
    "meta: ctime=1,rtime=2,uid=abcdefghijklmnopqrst",
    "meta:  ",
    "hookscript: local:snippets/x.sh",
    "lxc.hook.pre-start: /x",
    "lxc.mount.entry = /dev dev none bind",
    "lxc.idmap: u 0 0 65536",
    "env: LD_PRELOAD=/x",
    "dev0: /dev/sda",
    "args: -x",
    "unprivileged: 0",
    "unprivileged: 1",
    "onboot: 1",
    "features: nesting=1,keyctl=1,mount=nfs",
    "lock: backup",
    "unused0: local:vm-1-disk-9",
    "mp0 : local:x",
    "hostname:",
    "",
    "[snap]",
];

const VOLUME_PARTS: &[&str] = &[
    "local-lvm:vm-1-disk-0",
    "volume=local:x",
    "/srv",
    "/dev/sda",
    "mp=/data",
    "mp=/../etc",
    "size=8G",
    "size=65T",
    "backup=1",
    "ro=yes",
    "idmap=passthrough",
    "",
    "=x",
];

const NET_PARTS: &[&str] = &[
    "name=eth0",
    "bridge=vmbr0",
    "bridge=vmbr9",
    "hwaddr=AA:BB:CC:00:00:01",
    "firewall=1",
    "tag=40",
    "link_down=1",
    "type=veth",
    "type=phys",
    "ip=dhcp",
];

/// Hostile `pct.conf` lines: `volume=` anywhere, bind and device mount points, foreign bridges.
fn hostile_config() -> impl Strategy<Value = String> {
    let mount = (
        prop::sample::select(&["rootfs", "mp0", "mp1", "mp255", "mp01"][..]),
        prop::collection::vec(prop::sample::select(VOLUME_PARTS), 0..5),
    )
        .prop_map(|(key, parts)| format!("{key}: {}", parts.join(",")));
    let likely = (
        prop::sample::select(&["rootfs", "mp0", "mp1"][..]),
        prop::sample::select(&["local-lvm:vm-1-disk-0", "volume=local:x"][..]),
        prop::sample::select(&["size=1G", "size=1.5G", "size=64T"][..]),
        prop::option::of(prop::sample::select(VOLUME_PARTS)),
    )
        .prop_flat_map(|(key, volume, size, extra)| {
            let mut parts = vec![volume, size];
            if key != "rootfs" {
                parts.push("mp=/data");
            }
            parts.extend(extra);
            (Just(key), Just(parts).prop_shuffle())
        })
        .prop_map(|(key, parts)| format!("{key}: {}", parts.join(",")));
    let net = (
        prop::sample::select(&["net0", "net1", "net32"][..]),
        prop::collection::vec(prop::sample::select(NET_PARTS), 0..5),
    )
        .prop_map(|(key, parts)| format!("{key}: {}", parts.join(",")));
    let line = prop_oneof![
        4 => prop::sample::select(LINES).prop_map(str::to_owned),
        3 => mount,
        3 => likely,
        3 => net,
        1 => "[a-z]{1,8}: [ -~]{0,40}",
        1 => "[ -~]{300,400}".prop_map(|s| format!("hostname: {s}")),
    ];
    prop::collection::vec(line, 0..20).prop_map(|lines| lines.join("\n"))
}

async fn get(api: &Api, path: &str) -> (u16, Value) {
    let response = api.get(path).await;
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_preview_matches_filter() {
    let strategy = (
        hostile_config(),
        prop::option::of(Just("[OPTIONS]\nenable: 1\n")),
    );
    let mut runner = TestRunner::default();
    for backend in backends() {
        let name = backend.name();
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "pve1").await;
        seed_facts(&ts, &host, node_facts(CLUSTER, "pve1", vec![]), 0).await;
        let bridges = BTreeSet::from(["vmbr0".to_owned()]);
        for _ in 0..32 {
            let (config, firewall) = strategy.new_tree(&mut runner).unwrap().current();
            let snapshot = write_ct(&ts, &repo, &host, 200, &config, firewall).await;
            let (status, got) = get(
                &api,
                &format!(
                    "/api/v1/restore/snapshots/{snapshot}/container?host_id={}",
                    host.id
                ),
            )
            .await;
            assert_eq!(status, 200, "[{name}] {config:?}: {got}");
            let want = filter_ct(&FilterInput {
                config: config.as_bytes(),
                firewall: firewall.map(str::as_bytes),
                target: None,
                bridges: Some(&bridges),
            })
            .unwrap();
            let dropped: Vec<Value> = want
                .dropped
                .iter()
                .map(|d| json!({"key": d.key, "value": d.value, "reason": d.reason.as_str()}))
                .collect();
            let volumes: Vec<Value> = want
                .volumes
                .iter()
                .map(|v| json!({"key": v.key, "mp": v.mp, "size_bytes": v.size_bytes}))
                .collect();
            let network: Vec<Value> = want
                .network
                .iter()
                .map(|(key, value)| json!({"key": key, "value": value}))
                .collect();
            let kept = want.text.lines().filter(|l| !l.trim().is_empty()).count();
            let context = format!("[{name}] {config:?}");
            assert_eq!(got["dropped"], json!(dropped), "{context}");
            assert_eq!(got["volumes"], json!(volumes), "{context}");
            assert_eq!(got["network"], json!(network), "{context}");
            assert_eq!(got["kept_count"], kept, "{context}");
            assert_eq!(got["bridges_checked"], true, "{context}");
            assert_eq!(got["mode"], "snapshot", "{context}");
            assert_eq!(got["unprivileged"], true, "{context}");
            assert_eq!(
                got["idmap"], "u:0:100000:65536,g:0:100000:65536",
                "{context}"
            );
            assert_eq!(got["root_host_uid"], 100000, "{context}");
            assert_eq!(got["vmid"], 200, "{context}");
            assert_eq!(got["name"], "dns", "{context}");
        }
        ts.stop().await;
    }
}

async fn files_snapshot(ts: &TestServer, repo_id: &str) -> String {
    let store = LocalFsStore::new(ts.local_repo_root("main"));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    let mut writer = Writer::new(&store, keys, repo.config()).await.unwrap();
    let data = tempfile::tempdir_in(ts.dir.path()).unwrap();
    std::fs::write(data.path().join("a.txt"), b"alpha").unwrap();
    let outcome = writer
        .backup(
            &[data.path().to_path_buf()],
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    let snapshot = hex32(&outcome.snapshot_id);
    ingest_snapshot(ts.state(), repo_id, &snapshot, None)
        .await
        .unwrap();
    snapshot
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_preview_requires_read() {
    for backend in backends() {
        let name = backend.name();
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "pve1").await;
        let snapshot = write_ct(&ts, &repo, &host, 200, CONFIG, None).await;
        let files = files_snapshot(&ts, &repo).await;
        let preview = format!("/api/v1/restore/snapshots/{snapshot}/container");
        let targets = format!("/api/v1/restore/guest-targets?snapshot_id={snapshot}");

        let guest_only = common::bearer_client(&ts.issue_token(&["restore:guest"]).await);
        for path in [&preview, &targets] {
            let status = guest_only
                .get(format!("{}{path}", ts.rest_url()))
                .send()
                .await
                .unwrap()
                .status();
            assert_eq!(status, 403, "[{name}] {path}");
        }
        let read_only = common::bearer_client(&ts.issue_token(&["snapshots:read"]).await);
        let status = read_only
            .get(format!("{}{preview}", ts.rest_url()))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, 200, "[{name}]");

        for path in [
            format!("/api/v1/restore/snapshots/{files}/container"),
            format!("/api/v1/restore/guest-targets?snapshot_id={files}"),
        ] {
            let (status, body) = get(&api, &path).await;
            assert_eq!(status, 422, "[{name}] {path}");
            assert_eq!(body["code"], "restore_snapshot_not_ct", "[{name}]");
        }
        let (status, body) = get(&api, &preview).await;
        assert_eq!(status, 200, "[{name}] {body}");
        assert_eq!(body["bridges_checked"], false, "[{name}]");
        ts.stop().await;
    }
}

fn volume(key: &str, value: &str) -> ConfigLine {
    ConfigLine {
        key: key.into(),
        value: value.into(),
    }
}

const SAME_UID: &str = "abcdefghijklmnopqrst";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_targets_from_facts() {
    for backend in backends() {
        let name = backend.name();
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let one = enroll(&ts, &api, &repo, "pve-01").await;
        let two = enroll(&ts, &api, &repo, "pve-02").await;
        let other = enroll(&ts, &api, &repo, "pve-03").await;
        set_capabilities(&ts, &one, CAPS).await;

        let mut facts = node_facts(
            CLUSTER,
            "pve-01",
            vec![
                ProxmoxGuest {
                    running: true,
                    ha_managed: true,
                    pve_uid: "zzzzzzzzzzzzzzzzzzzz".into(),
                    ..ct(200)
                },
                ProxmoxGuest {
                    pve_uid: SAME_UID.into(),
                    ..ct(201)
                },
            ],
        );
        facts.guest_in_place_allowed = true;
        facts.nodes = vec![
            ClusterNode {
                name: "pve-01".into(),
                online: true,
                plugin_marker: "m".into(),
            },
            ClusterNode {
                name: "pve-04".into(),
                online: false,
                plugin_marker: String::new(),
            },
        ];
        seed_facts(&ts, &one, facts, 0).await;
        let mut facts = node_facts(CLUSTER, "pve-02", vec![ct(300)]);
        facts.plugin_installed = false;
        seed_facts(&ts, &two, facts, 0).await;
        let mut facts = node_facts("elsewhere", "pve-03", vec![ct(400)]);
        facts.guest_in_place_allowed = true;
        seed_facts(&ts, &other, facts, 0).await;

        let config = format!("{CONFIG}meta: uid={SAME_UID}\n");
        let snapshot = write_ct(&ts, &repo, &one, 201, &config, None).await;
        let (status, got) = get(
            &api,
            &format!("/api/v1/restore/guest-targets?snapshot_id={snapshot}"),
        )
        .await;
        assert_eq!(status, 200, "[{name}] {got}");
        assert_eq!(got["cluster"], CLUSTER, "[{name}]");
        assert_eq!(got["source_node"], "pve-01", "[{name}]");
        let nodes: Vec<(String, Value, bool, bool, bool)> = got["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| {
                (
                    n["name"].as_str().unwrap().to_string(),
                    n["host_id"].clone(),
                    n["agent"].as_bool().unwrap(),
                    n["plugin_installed"].as_bool().unwrap(),
                    n["in_place_allowed"].as_bool().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            nodes,
            [
                ("pve-01".into(), json!(one.id), true, true, true),
                ("pve-02".into(), json!(two.id), false, false, false),
                ("pve-04".into(), Value::Null, false, false, false),
            ],
            "[{name}]"
        );
        assert_eq!(got["nodes"][0]["next_vmid"], 105, "[{name}]");
        assert_eq!(
            got["storages"][&one.id],
            json!([{"id": "local-lvm", "type": "lvmthin", "shared": false}]),
            "[{name}]"
        );
        assert!(got["storages"].get(&other.id).is_none(), "[{name}]");
        let replace = got["replace"].as_array().unwrap();
        assert_eq!(replace.len(), 2, "[{name}] {replace:?}");
        assert_eq!(replace[0]["vmid"], 200, "[{name}]");
        assert_eq!(replace[0]["host_id"], json!(one.id), "[{name}]");
        assert_eq!(replace[0]["running"], true, "[{name}]");
        assert_eq!(replace[0]["ha_managed"], true, "[{name}]");
        assert_eq!(replace[0]["pve_uid"], "zzzzzzzzzzzzzzzzzzzz", "[{name}]");
        assert_eq!(replace[0]["other_guest"], true, "[{name}]");
        assert_eq!(replace[1]["vmid"], 201, "[{name}]");
        assert_eq!(replace[1]["other_guest"], false, "[{name}]");
        assert_eq!(got["snapshot_guest"]["uid"], SAME_UID, "[{name}]");
        assert_eq!(got["snapshot_guest"]["vmid"], 201, "[{name}]");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_targets_destroyed_volumes() {
    for backend in backends() {
        let name = backend.name();
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "pve1").await;
        let target = ProxmoxGuest {
            volumes: vec![
                volume("rootfs", "local-lvm:vm-300-disk-0,size=8G"),
                volume("mp0", "local-lvm:vm-300-disk-1,mp=/data,backup=1,size=8G"),
                volume("mp1", "local-lvm:vm-300-disk-2,mp=/big,backup=0,size=2T"),
                volume("mp2", "local-lvm:vm-300-disk-3,mp=/new,backup=1,size=1G"),
                volume("unused0", "local-lvm:vm-300-disk-9"),
            ],
            ..ct(300)
        };
        let mut facts = node_facts(CLUSTER, "pve1", vec![target]);
        facts.guest_in_place_allowed = true;
        seed_facts(&ts, &host, facts, 0).await;
        let config = "arch: amd64\nrootfs: local-lvm:vm-300-disk-0,size=8G\n\
                      mp0: local-lvm:vm-300-disk-1,mp=/data,backup=1,size=8G\n";
        let snapshot = write_ct(&ts, &repo, &host, 300, config, None).await;

        let (status, got) = get(
            &api,
            &format!("/api/v1/restore/guest-targets?snapshot_id={snapshot}"),
        )
        .await;
        assert_eq!(status, 200, "[{name}] {got}");
        let volumes: Vec<(String, bool, Value)> = got["replace"][0]["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| {
                (
                    v["key"].as_str().unwrap().to_string(),
                    v["destroyed_without_copy"].as_bool().unwrap(),
                    v["size_bytes"].clone(),
                )
            })
            .collect();
        assert_eq!(
            volumes,
            [
                ("rootfs".into(), false, json!(8u64 << 30)),
                ("mp0".into(), false, json!(8u64 << 30)),
                ("mp1".into(), true, json!(2u64 << 40)),
                ("mp2".into(), true, json!(1u64 << 30)),
                ("unused0".into(), true, Value::Null),
            ],
            "[{name}]"
        );
        assert_eq!(got["replace"][0]["volumes_truncated"], false, "[{name}]");
        ts.stop().await;
    }
}

const TARGET_UID: &str = "yyyyyyyyyyyyyyyyyyyy";

/// CT 200 on pve1, stopped, on a node that allows replacing it.
fn target(uid: &str) -> ProxmoxGuest {
    ProxmoxGuest {
        name: "dns".into(),
        pve_uid: uid.into(),
        pve_rtime: 1_700_000_000,
        volumes: vec![
            volume("rootfs", "local-lvm:vm-200-disk-0,size=8G"),
            volume("mp0", "local-lvm:vm-200-disk-1,mp=/data,backup=0,size=2G"),
        ],
        ..ct(200)
    }
}

fn replace_facts(guest: ProxmoxGuest) -> ProxmoxFacts {
    let mut facts = node_facts(CLUSTER, "pve1", vec![guest]);
    facts.guest_in_place_allowed = true;
    facts
}

/// A test server with pve1, CT 200 as `guest`, and a snapshot of CT 200 with `config`.
async fn replace_setup(
    backend: &common::Backend,
    guest: ProxmoxGuest,
    config: &str,
) -> (TestServer, Api, Host, String) {
    let ts = TestServer::start(backend).await;
    let api = ts.api().await;
    let repo = ts.create_local_repository("main").await;
    let host = enroll(&ts, &api, &repo, "pve1").await;
    set_capabilities(&ts, &host, CAPS).await;
    seed_facts(&ts, &host, replace_facts(guest), 0).await;
    let snapshot = write_ct(&ts, &repo, &host, 200, config, None).await;
    (ts, api, host, snapshot)
}

fn same_config() -> String {
    format!("{CONFIG}meta: uid={SAME_UID}\n")
}

fn replace_body(snapshot: &str, host: &Host) -> Value {
    json!({
        "snapshot_id": snapshot,
        "target_host_id": host.id,
        "storage": "local-lvm",
        "replace_vmid": 200,
    })
}

async fn queued_spec(ts: &TestServer, job_id: &str) -> RestoreSpec {
    let raw: Vec<u8> = sqlx::query("SELECT spec FROM jobs WHERE id = $1")
        .bind(job_id)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .get("spec");
    prost::Message::decode(raw.as_slice()).unwrap()
}

fn guest_target(spec: &RestoreSpec) -> GuestTarget {
    match &spec.target {
        Some(Target::Guest(target)) => target.clone(),
        other => panic!("not a guest target: {other:?}"),
    }
}

async fn post_as(ts: &TestServer, client: &reqwest::Client, body: &Value) -> (u16, Value) {
    let response = client
        .post(format!("{}/api/v1/restores/guest", ts.rest_url()))
        .json(body)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_restore_replace_scopes() {
    for backend in backends() {
        let name = backend.name();
        let (ts, api, host, snapshot) =
            replace_setup(&backend, target(SAME_UID), &same_config()).await;
        let body = replace_body(&snapshot, &host);

        let guest = common::bearer_client(&ts.issue_token(&["restore:guest"]).await);
        let (status, got) = post_as(&ts, &guest, &body).await;
        assert_eq!(status, 403, "[{name}] {got}");
        assert_eq!(
            got["message"],
            "The token needs the restore:guest-in-place scope to replace an existing container.",
            "[{name}]"
        );
        let in_place = common::bearer_client(&ts.issue_token(&["restore:guest-in-place"]).await);
        let (status, got) = post_as(&ts, &in_place, &new_body(&snapshot, &host)).await;
        assert_eq!(
            status, 403,
            "[{name}] new CT with restore:guest-in-place: {got}"
        );
        let files =
            common::bearer_client(&ts.issue_token(&["restore:host", "restore:in-place"]).await);
        let (status, _) = post_as(&ts, &files, &body).await;
        assert_eq!(status, 403, "[{name}] file scopes");
        assert_eq!(job_count(&ts).await, 0, "[{name}]");

        let (status, got) = post_as(&ts, &in_place, &body).await;
        assert_eq!(status, 201, "[{name}] {got}");
        let spec = queued_spec(&ts, got["job_id"].as_str().unwrap()).await;
        assert!(guest_target(&spec).in_place, "[{name}]");
        assert_eq!(guest_target(&spec).vmid, 200, "[{name}]");

        let no_csrf = reqwest::Client::new()
            .post(format!("{}/api/v1/restores/guest", ts.rest_url()))
            .header("cookie", api.cookie())
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(no_csrf.status(), 403, "[{name}] without CSRF");

        sqlx::query("UPDATE sessions SET authenticated_at = $1")
            .bind(now_ms() - 16 * 60_000)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        let before = job_count(&ts).await;
        let (status, got) = post(&api, &body).await;
        assert_eq!(status, 403, "[{name}] {got}");
        assert_eq!(got["code"], "step_up_required", "[{name}]");
        assert_eq!(job_count(&ts).await, before, "[{name}]");
        let (status, got) = post(&api, &new_body(&snapshot, &host)).await;
        assert_ne!(got["code"], "step_up_required", "[{name}] new CT: {status}");

        let step = api
            .post(
                "/api/v1/auth/step-up",
                &json!({ "password": common::ADMIN_PASSWORD }),
            )
            .await;
        assert_eq!(step.status(), 204, "[{name}]");
        let (status, got) = post(&api, &body).await;
        assert_eq!(status, 201, "[{name}] {got}");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_restore_replace_unverified() {
    for backend in backends() {
        let name = backend.name();
        let (ts, api, host, snapshot) =
            replace_setup(&backend, target(SAME_UID), &same_config()).await;
        sqlx::query("UPDATE snapshots SET verified_at = NULL WHERE id = $1")
            .bind(&snapshot)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        let mut body = replace_body(&snapshot, &host);
        body["accept_unverified"] = json!(true);
        let (status, got) = post(&api, &body).await;
        assert_eq!(status, 400, "[{name}] {got}");
        assert_eq!(got["code"], "restore_unverified_in_place", "[{name}]");

        let (status, got) = post(&api, &new_body(&snapshot, &host)).await;
        assert_eq!(status, 400, "[{name}] {got}");
        assert_eq!(got["code"], "restore_unverified_not_confirmed", "[{name}]");
        let mut accepted = new_body(&snapshot, &host);
        accepted["accept_unverified"] = json!(true);
        let (status, got) = post(&api, &accepted).await;
        assert_eq!(status, 201, "[{name}] {got}");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_restore_replace_expected() {
    for backend in backends() {
        let name = backend.name();
        let (ts, api, host, snapshot) =
            replace_setup(&backend, target(SAME_UID), &same_config()).await;
        let (status, got) = post(&api, &replace_body(&snapshot, &host)).await;
        assert_eq!(status, 201, "[{name}] {got}");
        let job_id = got["job_id"].as_str().unwrap().to_string();
        let expected = guest_target(&queued_spec(&ts, &job_id).await)
            .expected
            .expect("expected target");
        assert_eq!(expected.vmid, 200, "[{name}]");
        assert_eq!(expected.name, "dns", "[{name}]");
        assert_eq!(expected.pve_uid, SAME_UID, "[{name}]");
        assert_eq!(expected.pve_rtime, 1_700_000_000, "[{name}]");
        assert_eq!(expected.instance_uuid, uuid_of(200), "[{name}]");
        assert_eq!(expected.volumes, target(SAME_UID).volumes, "[{name}]");
        let record: Value = serde_json::from_str(
            &sqlx::query("SELECT restore_json FROM jobs WHERE id = $1")
                .bind(&job_id)
                .fetch_one(&ts.state().db.pool)
                .await
                .unwrap()
                .get::<String, _>("restore_json"),
        )
        .unwrap();
        assert_eq!(record["guest"]["expected"]["pve_uid"], SAME_UID, "[{name}]");
        assert_eq!(record["guest"]["replace"], true, "[{name}]");

        seed_facts(&ts, &host, replace_facts(target(SAME_UID)), 16 * 60_000).await;
        let (status, got) = post(&api, &replace_body(&snapshot, &host)).await;
        assert_eq!(status, 409, "[{name}] {got}");
        assert_eq!(got["code"], "guest_facts_stale", "[{name}]");

        let cut = ProxmoxGuest {
            volumes_truncated: true,
            ..target(SAME_UID)
        };
        seed_facts(&ts, &host, replace_facts(cut), 0).await;
        let (status, got) = post(&api, &replace_body(&snapshot, &host)).await;
        assert_eq!(status, 422, "[{name}] {got}");
        assert_eq!(got["code"], "guest_target_unverifiable", "[{name}]");

        let mut closed = replace_facts(target(SAME_UID));
        closed.guest_in_place_allowed = false;
        seed_facts(&ts, &host, closed, 0).await;
        let (status, got) = post(&api, &replace_body(&snapshot, &host)).await;
        assert_eq!(status, 400, "[{name}] {got}");
        assert_eq!(got["code"], "guest_in_place_disabled", "[{name}]");
        assert_eq!(job_count(&ts).await, 1, "[{name}]");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_restore_other_guest() {
    for backend in backends() {
        let name = backend.name();
        let (ts, api, host, other) =
            replace_setup(&backend, target(TARGET_UID), &same_config()).await;
        let (status, got) = post(&api, &replace_body(&other, &host)).await;
        assert_eq!(status, 400, "[{name}] {got}");
        assert_eq!(got["code"], "restore_other_guest_not_confirmed", "[{name}]");
        let mut confirmed = replace_body(&other, &host);
        confirmed["confirm_other_guest"] = json!(true);
        let (status, got) = post(&api, &confirmed).await;
        assert_eq!(status, 201, "[{name}] {got}");
        let pair = guest_target(&queued_spec(&ts, got["job_id"].as_str().unwrap()).await)
            .confirm_other_guest
            .unwrap();
        assert_eq!(
            (pair.snapshot_pve_uid.as_str(), pair.target_pve_uid.as_str()),
            (SAME_UID, TARGET_UID),
            "[{name}]"
        );

        let repo: String = sqlx::query("SELECT repository_id FROM snapshots WHERE id = $1")
            .bind(&other)
            .fetch_one(&ts.state().db.pool)
            .await
            .unwrap()
            .get("repository_id");
        let no_uid = write_ct(&ts, &repo, &host, 200, CONFIG, None).await;
        let (status, got) = post(&api, &replace_body(&no_uid, &host)).await;
        assert_eq!(status, 400, "[{name}] {got}");
        assert_eq!(got["code"], "restore_other_guest_not_confirmed", "[{name}]");
        let mut confirmed = replace_body(&no_uid, &host);
        confirmed["confirm_other_guest"] = json!(true);
        let (status, got) = post(&api, &confirmed).await;
        assert_eq!(status, 201, "[{name}] {got}");
        let pair = guest_target(&queued_spec(&ts, got["job_id"].as_str().unwrap()).await)
            .confirm_other_guest
            .unwrap();
        assert_eq!(
            (pair.snapshot_pve_uid.as_str(), pair.target_pve_uid.as_str()),
            ("", TARGET_UID),
            "[{name}]"
        );

        seed_facts(&ts, &host, replace_facts(target(SAME_UID)), 0).await;
        let mut confirmed = replace_body(&other, &host);
        confirmed["confirm_other_guest"] = json!(true);
        let (status, got) = post(&api, &confirmed).await;
        assert_eq!(status, 201, "[{name}] {got}");
        let spec = queued_spec(&ts, got["job_id"].as_str().unwrap()).await;
        assert!(
            guest_target(&spec).confirm_other_guest.is_none(),
            "[{name}] the same guest needs no confirmation"
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_restore_replace_ha_and_running() {
    for backend in backends() {
        let name = backend.name();
        let ha = ProxmoxGuest {
            ha_managed: true,
            ..target(SAME_UID)
        };
        let (ts, api, host, snapshot) = replace_setup(&backend, ha, &same_config()).await;
        let (status, got) = post(&api, &replace_body(&snapshot, &host)).await;
        assert_eq!(status, 422, "[{name}] {got}");
        assert_eq!(got["code"], "guest_ha_managed", "[{name}]");

        let running = ProxmoxGuest {
            running: true,
            ..target(SAME_UID)
        };
        seed_facts(&ts, &host, replace_facts(running), 0).await;
        let (status, got) = post(&api, &replace_body(&snapshot, &host)).await;
        assert_eq!(status, 409, "[{name}] {got}");
        assert_eq!(got["code"], "guest_running", "[{name}]");
        assert_eq!(job_count(&ts).await, 0, "[{name}]");
        ts.stop().await;
    }
}

async fn age_job(ts: &TestServer, job_id: &str, by_ms: i64) {
    sqlx::query("UPDATE jobs SET queued_at = queued_at - $1 WHERE id = $2")
        .bind(by_ms)
        .bind(job_id)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_restore_replace_expires() {
    for backend in backends() {
        let name = backend.name();
        let (ts, api, host, snapshot) =
            replace_setup(&backend, target(SAME_UID), &same_config()).await;
        let (status, got) = post(&api, &replace_body(&snapshot, &host)).await;
        assert_eq!(status, 201, "[{name}] {got}");
        let replace = got["job_id"].as_str().unwrap().to_string();
        age_job(&ts, &replace, 15 * 60_000 + 1000).await;

        let mut agent = Fake::connect_with(&ts, &host, &AGENT_CAPS).await;
        let assigned = agent.drain(std::time::Duration::from_secs(3)).await;
        assert!(
            assigned.into_iter().all(|m| assigned_restore(m).is_none()),
            "[{name}] the expired replace was handed out"
        );
        let job: Value = api
            .get(&format!("/api/v1/jobs/{replace}"))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(job["state"], "failed", "[{name}] {job}");
        assert_eq!(job["error_code"], "restore_expired", "[{name}]");
        assert_eq!(
            job["error_message"],
            "The replace was confirmed more than 15 minutes ago. Confirm it again.",
            "[{name}]"
        );

        let (status, got) = post(&api, &new_body(&snapshot, &host)).await;
        assert_eq!(status, 201, "[{name}] {got}");
        let new = got["job_id"].as_str().unwrap().to_string();
        age_job(&ts, &new, 15 * 60_000 + 1000).await;
        let (assigned, _) = agent.recv(assigned_restore).await;
        assert_eq!(assigned, new, "[{name}] a new container does not expire");
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_restore_vmid_taken() {
    for backend in backends() {
        let name = backend.name();
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "pve1").await;
        let peer = enroll(&ts, &api, &repo, "pve2").await;
        set_capabilities(&ts, &host, CAPS).await;
        seed_facts(&ts, &host, node_facts(CLUSTER, "pve1", vec![]), 0).await;
        seed_facts(&ts, &peer, node_facts(CLUSTER, "pve2", vec![ct(200)]), 0).await;
        let snapshot = write_ct(&ts, &repo, &host, 200, CONFIG, None).await;
        let mut body = new_body(&snapshot, &host);
        body["vmid"] = json!(200);
        let (status, got) = post(&api, &body).await;
        assert_eq!(status, 409, "[{name}] {got}");
        assert_eq!(got["code"], "restore_vmid_taken", "[{name}]");
        let mut storage = new_body(&snapshot, &host);
        storage["storage"] = json!("local");
        let (status, got) = post(&api, &storage).await;
        assert_eq!(status, 422, "[{name}] {got}");
        assert_eq!(got["code"], "restore_storage_invalid", "[{name}]");
        let mut both = new_body(&snapshot, &host);
        both["replace_vmid"] = json!(200);
        let (status, _) = post(&api, &both).await;
        assert_eq!(status, 422, "[{name}] vmid and replace_vmid");
        assert_eq!(job_count(&ts).await, 0, "[{name}]");
        ts.stop().await;
    }
}
