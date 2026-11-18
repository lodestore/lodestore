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
