use crate::common;

use std::os::fd::OwnedFd;

use common::fake_agent::{Fake, Host, is_ack};
use common::{Api, TestServer, backends};
use lodestore_agent::{EnrollArgs, NetOptions};
use lodestore_format::ct::{CtMeta, CtMode, IdMap};
use lodestore_format::index::hex32;
use lodestore_format::repo::Repository;
use lodestore_format::source::{Meta, SourceKey};
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, CtBackup, Writer};
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::restore_spec::Target;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{
    DroppedConfigKey, GuestKind, GuestRestoreResult, GuestTarget, JobAccepted, JobFinished,
    JobOutcome, ProxmoxFacts, ProxmoxGuest, ProxmoxStorage, RestoreSpec, RestoreStats, SourceFacts,
};
use lodestore_server::catalog::ingest_snapshot;
use lodestore_server::db::now_ms;
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
const CONFIG: &str = "arch: amd64\nhostname: dns\nrootfs: local-lvm:vm-200-disk-0,size=8G\n\
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
            idmap: Some(IdMap::default_unprivileged()),
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
