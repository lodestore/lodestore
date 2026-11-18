use crate::common;

use std::time::Duration;

use common::fake_agent::{Fake, Host};
use common::{Api, TestServer, backends, wait_until};
use lodestore_agent::{EnrollArgs, NetOptions};
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::{
    Database, DatabaseKind, DatabaseServer, GuestKind, Heartbeat, ProxmoxFacts, ProxmoxGuest,
    SourceFacts,
};
use lodestore_server::db::now_ms;
use lodestore_server::sources::{self, Discovery};
use sqlx::{AnyPool, Row};

const UUID_A: &str = "11111111-1111-4111-8111-111111111111";
const UUID_B: &str = "22222222-2222-4222-8222-222222222222";

async fn enroll(ts: &TestServer, api: &Api, repo: &str, hostname: &str) -> Host {
    let token = api.join_token(repo, serde_json::json!([])).await;
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

fn guest(kind: GuestKind, vmid: u32, name: &str, uuid: &str) -> ProxmoxGuest {
    ProxmoxGuest {
        kind: kind as i32,
        vmid,
        name: name.to_string(),
        uuid: uuid.to_string(),
        ..Default::default()
    }
}

fn node(guests: Vec<ProxmoxGuest>) -> SourceFacts {
    SourceFacts {
        age_ms: 1000,
        proxmox: Some(ProxmoxFacts {
            cluster: "pve-prod".to_string(),
            node: "pve1".to_string(),
            guests,
        }),
        ..Default::default()
    }
}

async fn report(host: &Host, ts: &TestServer, facts: Option<SourceFacts>) -> Fake {
    let agent = Fake::connect(ts, host, vec![]).await;
    agent
        .send(AgentBody::Heartbeat(Heartbeat {
            source_facts: facts,
            ..Default::default()
        }))
        .await;
    agent
}

async fn seen(pool: &AnyPool, host: &str) -> bool {
    sqlx::query("SELECT last_seen_at FROM agents WHERE id = $1")
        .bind(host)
        .fetch_one(pool)
        .await
        .unwrap()
        .try_get::<Option<i64>, _>(0)
        .unwrap()
        .is_some()
}

async fn detail_types(api: &Api, host: &str) -> Vec<String> {
    let body: serde_json::Value = api
        .get(&format!("/api/v1/hosts/{host}"))
        .await
        .json()
        .await
        .unwrap();
    body["source_types"]
        .as_array()
        .expect("source_types")
        .iter()
        .map(|t| t.as_str().unwrap().to_string())
        .collect()
}

async fn source_types(api: &Api, host: &str, want: &[&str]) -> Vec<String> {
    wait_until(Duration::from_secs(5), || async {
        detail_types(api, host).await == want
    })
    .await;
    detail_types(api, host).await
}

async fn collected_at(pool: &AnyPool, host: &str) -> Option<i64> {
    sqlx::query("SELECT sources_collected_at FROM agents WHERE id = $1")
        .bind(host)
        .fetch_one(pool)
        .await
        .unwrap()
        .try_get::<Option<i64>, _>(0)
        .unwrap()
}

async fn refs(pool: &AnyPool, host: &str) -> Vec<String> {
    sqlx::query("SELECT source_ref FROM discovered_sources WHERE host_id = $1 ORDER BY source_ref")
        .bind(host)
        .fetch_all(pool)
        .await
        .unwrap()
        .iter()
        .map(|row| row.try_get::<String, _>(0).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sources_heartbeat_reaches_host_detail() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let pool = &ts.state().db.pool;

        let a = enroll(&ts, &api, &repo, "host-a").await;
        let _a = report(
            &a,
            &ts,
            Some(node(vec![
                guest(GuestKind::Vm, 101, "web-01", UUID_A),
                guest(GuestKind::Ct, 200, "dns", UUID_B),
            ])),
        )
        .await;
        assert_eq!(
            source_types(&api, &a.id, &["proxmox-vm", "proxmox-ct"]).await,
            ["proxmox-vm", "proxmox-ct"]
        );
        let collected = collected_at(pool, &a.id).await.unwrap();
        assert!(collected <= now_ms() - 1000, "{collected}");
        assert_eq!(
            refs(pool, &a.id).await,
            [
                format!("proxmox-ct/pve-prod/{UUID_B}"),
                format!("proxmox-vm/pve-prod/{UUID_A}"),
            ]
        );

        let b = enroll(&ts, &api, &repo, "host-b").await;
        let _b = report(&b, &ts, None).await;
        assert!(wait_until(Duration::from_secs(5), || seen(pool, &b.id)).await);
        assert!(source_types(&api, &b.id, &[]).await.is_empty());
        assert_eq!(collected_at(pool, &b.id).await, None);

        let c = enroll(&ts, &api, &repo, "host-c").await;
        let _c = report(&c, &ts, Some(SourceFacts::default())).await;
        assert!(
            wait_until(Duration::from_secs(5), || async {
                collected_at(pool, &c.id).await.is_some()
            })
            .await
        );
        assert!(source_types(&api, &c.id, &[]).await.is_empty());

        let d = enroll(&ts, &api, &repo, "host-d").await;
        let _d = report(
            &d,
            &ts,
            Some(node(vec![guest(GuestKind::Vm, 101, "web-01", UUID_A)])),
        )
        .await;
        assert_eq!(
            source_types(&api, &d.id, &["proxmox-vm"]).await,
            ["proxmox-vm"]
        );

        let e = enroll(&ts, &api, &repo, "host-e").await;
        let _e = report(&e, &ts, Some(node(vec![]))).await;
        assert!(
            wait_until(Duration::from_secs(5), || async {
                collected_at(pool, &e.id).await.is_some()
            })
            .await
        );
        assert!(source_types(&api, &e.id, &[]).await.is_empty());

        ts.stop().await;
    }
}

async fn stored_json(pool: &AnyPool, host: &str) -> Option<String> {
    sqlx::query("SELECT sources_json FROM agents WHERE id = $1")
        .bind(host)
        .fetch_one(pool)
        .await
        .unwrap()
        .try_get::<Option<String>, _>(0)
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sources_rewrites_only_on_change() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let pool = &ts.state().db.pool;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let now = now_ms();
        let mut last: Option<Discovery> = None;

        let a = node(vec![guest(GuestKind::Vm, 101, "web-01", UUID_A)]);
        sources::record(ts.state(), &host.id, &a, &mut last, now)
            .await
            .unwrap();
        let first = stored_json(pool, &host.id).await.unwrap();
        assert_eq!(collected_at(pool, &host.id).await, Some(now - 1000));

        sources::record(ts.state(), &host.id, &a, &mut last, now + 5000)
            .await
            .unwrap();
        assert_eq!(stored_json(pool, &host.id).await.unwrap(), first);
        assert_eq!(collected_at(pool, &host.id).await, Some(now + 4000));

        let b = node(vec![guest(GuestKind::Vm, 102, "db-01", UUID_B)]);
        sources::record(ts.state(), &host.id, &b, &mut last, now + 6000)
            .await
            .unwrap();
        assert_eq!(
            refs(pool, &host.id).await,
            [format!("proxmox-vm/pve-prod/{UUID_B}")]
        );

        let postgres = SourceFacts {
            databases: vec![DatabaseServer {
                kind: DatabaseKind::Postgres as i32,
                port: 5432,
                server_id: "7".to_string(),
                databases: vec![
                    Database {
                        name: "app".to_string(),
                        oid: 16384,
                    },
                    Database {
                        name: "billing".to_string(),
                        oid: 16385,
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        sources::record(ts.state(), &host.id, &postgres, &mut last, now + 7000)
            .await
            .unwrap();
        let stored = sources::load(ts.state(), &host.id).await.unwrap().unwrap();
        assert_eq!(stored.discovery.types(), ["postgres"]);
        assert_eq!(
            refs(pool, &host.id).await,
            ["postgres/7/16384", "postgres/7/16385"]
        );

        let forged = node(vec![
            guest(GuestKind::Vm, 101, "a", UUID_A),
            guest(GuestKind::Vm, 102, "b", UUID_A),
        ]);
        sources::record(ts.state(), &host.id, &forged, &mut last, now + 8000)
            .await
            .unwrap();
        assert_eq!(refs(pool, &host.id).await.len(), 1);
        let stored = sources::load(ts.state(), &host.id).await.unwrap().unwrap();
        assert!(stored.discovery.truncated);

        ts.stop().await;
    }
}

fn yaml_policy(name: &str, host: &str, spec: &str) -> String {
    let version = if spec.contains("source:") {
        "v1alpha2"
    } else {
        "v1alpha1"
    };
    format!(
        "apiVersion: lodestore.io/{version}\nkind: BackupPolicy\nmetadata:\n  name: {name}\n\
         spec:\n  repository: main\n  selector:\n    hosts: [{host}]\n{spec}"
    )
}

async fn seed_policy(pool: &AnyPool, repo: &str, name: &str, host: &str, spec: &str) {
    let yaml = yaml_policy(name, host, spec);
    let policy = lodestore_policy::parse_yaml(&yaml).unwrap();
    sqlx::query(
        "INSERT INTO policies (id, name, repository_id, yaml, spec_json, revision, \
         created_at, updated_at) VALUES ($1, $1, $2, $3, $4, 1, $5, $5)",
    )
    .bind(name)
    .bind(repo)
    .bind(&yaml)
    .bind(serde_json::to_string(&policy).unwrap())
    .bind(now_ms())
    .execute(pool)
    .await
    .unwrap();
}

async fn set_capabilities(pool: &AnyPool, host: &str, caps: &[&str]) {
    sqlx::query("UPDATE agents SET capabilities_json = $1 WHERE id = $2")
        .bind(serde_json::to_string(caps).unwrap())
        .bind(host)
        .execute(pool)
        .await
        .unwrap();
}

async fn sources_of(api: &Api, host: &str) -> serde_json::Value {
    let response = api.get(&format!("/api/v1/hosts/{host}/sources")).await;
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

fn protection(item: &serde_json::Value) -> (&str, Vec<&str>) {
    (
        item["protection"]["state"].as_str().unwrap(),
        item["protection"]["policies"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap())
            .collect(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sources_tab_lists_guests_with_protection() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let pool = &ts.state().db.pool;
        let host = enroll(&ts, &api, &repo, "pve1").await;

        let mut web = guest(GuestKind::Vm, 101, "web-01", UUID_A);
        web.pool = "prod".to_string();
        web.tags = vec!["web".to_string()];
        let facts = node(vec![
            guest(GuestKind::Ct, 200, "dns", UUID_B),
            guest(
                GuestKind::Vm,
                103,
                "db-01",
                "33333333-3333-4333-8333-333333333333",
            ),
            web,
        ]);
        let mut last = None;
        sources::record(ts.state(), &host.id, &facts, &mut last, now_ms())
            .await
            .unwrap();

        let body = sources_of(&api, &host.id).await;
        assert_eq!(
            body["scope"],
            serde_json::json!({ "kind": "cluster", "name": "pve-prod" })
        );
        assert_eq!(body["truncated"], false);
        assert!(body["collected_at"].is_i64());
        assert!(body["files"].is_null());
        let items = body["items"].as_array().unwrap();
        let labels: Vec<_> = items.iter().map(|i| i["label"].as_str().unwrap()).collect();
        assert_eq!(
            labels,
            ["VM 101 · web-01", "VM 103 · db-01", "CT 200 · dns"]
        );
        assert_eq!(items[0]["pool"], "prod");
        assert_eq!(items[0]["tags"], serde_json::json!(["web"]));
        assert_eq!(items[0]["vmid"], 101);
        assert_eq!(items[0]["source_type"], "proxmox-vm");
        for item in items {
            assert_eq!(protection(item), ("agent_too_old", vec![]));
            assert!(!item.to_string().contains("proxmox-vm/"));
            assert!(!item.to_string().contains(UUID_A));
        }

        set_capabilities(pool, &host.id, &["source:proxmox-vm", "source:proxmox-ct"]).await;
        seed_policy(
            pool,
            &repo,
            "vms",
            &host.id,
            "  source:\n    type: proxmox-vm\n    vmids: [101]\n",
        )
        .await;
        let body = sources_of(&api, &host.id).await;
        let items = body["items"].as_array().unwrap();
        assert_eq!(protection(&items[0]), ("protected", vec!["vms"]));
        assert_eq!(protection(&items[1]), ("unprotected", vec![]));
        assert_eq!(protection(&items[2]), ("unprotected", vec![]));

        seed_policy(pool, &repo, "etc", &host.id, "  paths: [/etc]\n").await;
        let body = sources_of(&api, &host.id).await;
        assert_eq!(body["files"]["policies"], serde_json::json!(["etc"]));
        assert_eq!(body["files"]["paths"], serde_json::json!(["/etc"]));
        assert!(body["files"]["last_snapshot_at"].is_null());

        ts.stop().await;
    }
}

fn postgres_server(port: u32, server_id: &str, names: &[(&str, u32)]) -> DatabaseServer {
    DatabaseServer {
        kind: DatabaseKind::Postgres as i32,
        port,
        version: "16.4".to_string(),
        server_id: server_id.to_string(),
        databases: names
            .iter()
            .map(|(name, oid)| Database {
                name: name.to_string(),
                oid: *oid,
            })
            .collect(),
        ..Default::default()
    }
}

async fn seed_snapshot(pool: &AnyPool, repo: &str, host: &str, source_ref: &str, received: i64) {
    sqlx::query(
        "INSERT INTO snapshots (id, repository_id, host_id, hostname, time_ms, paths_json, \
         tags_json, incomplete, errors_total, warnings_total, stats_json, discovered_at, \
         received_at, verified_at, source_ref) \
         VALUES ($1, $2, $3, 'inserted', $4, '[]', '[]', 0, 0, 0, '{}', $4, $4, $4, $5)",
    )
    .bind("a".repeat(64))
    .bind(repo)
    .bind(host)
    .bind(received)
    .bind(source_ref)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sources_tab_groups_databases_and_clones() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let pool = &ts.state().db.pool;
        let db02 = enroll(&ts, &api, &repo, "db-02").await;
        let db03 = enroll(&ts, &api, &repo, "db-03").await;

        let unreachable = DatabaseServer {
            kind: DatabaseKind::Postgres as i32,
            port: 5433,
            version: "14.2".to_string(),
            unreachable: true,
            ..Default::default()
        };
        let facts = SourceFacts {
            databases: vec![
                unreachable,
                postgres_server(5432, "7", &[("billing", 16385), ("app", 16384)]),
            ],
            ..Default::default()
        };
        let clone = SourceFacts {
            databases: vec![postgres_server(5432, "7", &[("app", 16384)])],
            ..Default::default()
        };
        let mut last = None;
        sources::record(ts.state(), &db02.id, &facts, &mut last, now_ms())
            .await
            .unwrap();
        sources::record(ts.state(), &db03.id, &clone, &mut None, now_ms())
            .await
            .unwrap();
        set_capabilities(pool, &db02.id, &["source:postgres"]).await;
        seed_policy(
            pool,
            &repo,
            "pg",
            &db02.id,
            "  source:\n    type: postgres\n    databases: [app]\n",
        )
        .await;
        let received = now_ms() - 5000;
        seed_snapshot(pool, &repo, &db02.id, "postgres/7/16384", received).await;

        let body = sources_of(&api, &db02.id).await;
        assert!(body["scope"].is_null());
        let groups = body["groups"].as_array().unwrap();
        let ids: Vec<_> = groups.iter().map(|g| g["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["postgres:5432", "postgres:5433"]);
        assert_eq!(groups[0]["unreachable"], false);
        assert_eq!(groups[0]["version"], "16.4");
        assert_eq!(groups[0]["port"], 5432);
        assert_eq!(groups[0]["shared_with"], serde_json::json!(["db-03"]));
        assert_eq!(groups[1]["unreachable"], true);
        assert_eq!(groups[1]["shared_with"], serde_json::json!([]));

        let items = body["items"].as_array().unwrap();
        let names: Vec<_> = items.iter().map(|i| i["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["app", "billing"]);
        assert_eq!(items[0]["label"], "app · PostgreSQL 16");
        assert_eq!(items[0]["group_id"], "postgres:5432");
        assert_eq!(items[0]["oid"], 16384);
        assert_eq!(items[0]["source_type"], "postgres");
        assert_eq!(items[0]["shared_with"], serde_json::json!(["db-03"]));
        assert_eq!(items[1]["shared_with"], serde_json::json!([]));
        assert_eq!(items[0]["last_snapshot_at"], received);
        assert!(items[1]["last_snapshot_at"].is_null());
        assert_eq!(protection(&items[0]), ("protected", vec!["pg"]));
        assert_eq!(protection(&items[1]), ("unprotected", vec![]));
        assert!(!body.to_string().contains("postgres/7/"));

        sqlx::query("UPDATE agents SET status = 'revoked' WHERE id = $1")
            .bind(&db03.id)
            .execute(pool)
            .await
            .unwrap();
        let body = sources_of(&api, &db02.id).await;
        assert_eq!(body["groups"][0]["shared_with"], serde_json::json!([]));
        assert_eq!(body["items"][0]["shared_with"], serde_json::json!([]));

        ts.stop().await;
    }
}
