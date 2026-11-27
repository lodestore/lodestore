//! What an agent says about its host is a claim, not a right: a forged or bloated
//! source list, or a forged capability, may narrow what the server does for that
//! host (a refused job) but never widens a scope, changes a policy or a selector,
//! reaches the signed provenance, or costs more than the stored limits.

use crate::common;

use std::time::Duration;

use common::fake_agent::{Fake, Host};
use common::{Api, TestServer, backends, wait_until};
use lodestore_agent::{EnrollArgs, NetOptions};
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{
    DatabaseKind, DatabaseServer, GuestKind, Heartbeat, JobSpec, ProxmoxFacts, ProxmoxGuest,
    SourceFacts,
};
use lodestore_server::db::now_ms;
use lodestore_server::sources::{
    self, Discovery, MAX_DATABASES, MAX_GUEST_TAGS, MAX_GUESTS, MAX_NAME_BYTES, MAX_SERVERS,
};
use sqlx::{AnyPool, Row};

const VM: &str = "source:proxmox-vm";
const SHARED_UUID: &str = "11111111-1111-4111-8111-111111111111";

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

fn guest(vmid: u32, uuid: &str) -> ProxmoxGuest {
    ProxmoxGuest {
        kind: GuestKind::Vm as i32,
        vmid,
        name: format!("vm-{vmid}"),
        uuid: uuid.to_string(),
        ..Default::default()
    }
}

fn facts(guests: Vec<ProxmoxGuest>) -> SourceFacts {
    SourceFacts {
        age_ms: 1000,
        proxmox: Some(ProxmoxFacts {
            cluster: "pve-prod".to_string(),
            node: "pve".to_string(),
            guests,
            ..Default::default()
        }),
        ..Default::default()
    }
}

async fn policy(pool: &AnyPool, repo: &str, name: &str, host: &Host, spec: &str) {
    let version = if spec.contains("source:") {
        "v1alpha2"
    } else {
        "v1alpha1"
    };
    let yaml = format!(
        "apiVersion: lodestore.io/{version}\nkind: BackupPolicy\nmetadata:\n  name: {name}\n\
         spec:\n  repository: main\n  selector:\n    hosts: [{}]\n{spec}",
        host.id
    );
    let parsed = lodestore_policy::parse_yaml(&yaml).unwrap();
    sqlx::query(
        "INSERT INTO policies (id, name, repository_id, yaml, spec_json, revision, \
         created_at, updated_at) VALUES ($1, $1, $2, $3, $4, 1, $5, $5)",
    )
    .bind(name)
    .bind(repo)
    .bind(&yaml)
    .bind(serde_json::to_string(&parsed).unwrap())
    .bind(now_ms())
    .execute(pool)
    .await
    .unwrap();
}

async fn rights(pool: &AnyPool) -> String {
    let mut out = String::new();
    let policies = sqlx::query("SELECT name, yaml, spec_json FROM policies ORDER BY name")
        .fetch_all(pool)
        .await
        .unwrap();
    for row in policies {
        for column in ["name", "yaml", "spec_json"] {
            out.push_str(&row.try_get::<String, _>(column).unwrap());
            out.push('\n');
        }
    }
    let tokens = sqlx::query("SELECT id, scopes_json FROM api_tokens ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap();
    for row in tokens {
        for column in ["id", "scopes_json"] {
            out.push_str(&row.try_get::<String, _>(column).unwrap());
            out.push('\n');
        }
    }
    out
}

async fn stored(pool: &AnyPool, host: &str) -> Option<String> {
    sqlx::query("SELECT sources_json FROM agents WHERE id = $1")
        .bind(host)
        .fetch_one(pool)
        .await
        .unwrap()
        .try_get::<Option<String>, _>(0)
        .unwrap()
}

async fn wait_stored(pool: &AnyPool, host: &str, ready: impl Fn(&Discovery) -> bool) {
    let reached = wait_until(Duration::from_secs(10), || async {
        stored(pool, host)
            .await
            .and_then(|json| serde_json::from_str::<Discovery>(&json).ok())
            .is_some_and(|d| ready(&d))
    })
    .await;
    assert!(reached, "the server never stored the expected sources");
}

fn assigned(body: ServerBody) -> Option<JobSpec> {
    match body {
        ServerBody::JobAssign(a) => a.spec,
        _ => None,
    }
}

async fn heartbeat(agent: &Fake, facts: SourceFacts) {
    agent
        .send(AgentBody::Heartbeat(Heartbeat {
            source_facts: Some(facts),
            ..Default::default()
        }))
        .await;
}

async fn run(api: &Api, name: &str) -> String {
    let response = api
        .post(
            &format!("/api/v1/policies/{name}/run"),
            &serde_json::json!({}),
        )
        .await;
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    body["jobs"][0]["id"].as_str().unwrap().to_string()
}

async fn job_column(pool: &AnyPool, id: &str, column: &'static str) -> Option<String> {
    let sql = format!("SELECT {column} FROM jobs WHERE id = $1");
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
        .try_get::<Option<String>, _>(0)
        .unwrap()
}

async fn wait_job(pool: &AnyPool, id: &str, state: &str) {
    let reached = wait_until(Duration::from_secs(10), || async {
        job_column(pool, id, "state").await.as_deref() == Some(state)
    })
    .await;
    assert!(
        reached,
        "job {id} is {:?}, expected {state}",
        job_column(pool, id, "state").await
    );
}

async fn finish(pool: &AnyPool, id: &str) {
    sqlx::query("UPDATE jobs SET state = 'succeeded', finished_at = $1 WHERE id = $2")
        .bind(now_ms())
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_source_facts_never_widen_rights() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let pool = &ts.state().db.pool;
        let a = enroll(&ts, &api, &repo, "pve-a").await;
        let b = enroll(&ts, &api, &repo, "pve-b").await;
        let vms = "  source:\n    type: proxmox-vm\n    vmids: [101]\n";
        policy(pool, &repo, "files-a", &a, "  paths: [/etc]\n").await;
        policy(pool, &repo, "vms-a", &a, vms).await;
        policy(pool, &repo, "vms-b", &b, vms).await;
        ts.issue_token(&["hosts:read"]).await;
        let before = rights(pool).await;

        // A forges a guest of B and a capability it never needed.
        let mut last = None;
        sources::record(
            ts.state(),
            &b.id,
            &facts(vec![guest(101, SHARED_UUID)]),
            &mut last,
            now_ms(),
        )
        .await
        .unwrap();
        let mut agent = Fake::connect_with(&ts, &a, &[VM]).await;
        heartbeat(&agent, facts(vec![guest(101, SHARED_UUID)])).await;
        wait_stored(pool, &a.id, |d| d.proxmox.is_some()).await;

        let tab: serde_json::Value = api
            .get(&format!("/api/v1/hosts/{}/sources", b.id))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(
            tab["items"][0]["shared_with"],
            serde_json::json!(["pve-a"]),
            "[{}]",
            backend.name()
        );

        // A files job is what it was: no source, no instance reference.
        let files_job = run(&api, "files-a").await;
        let spec = agent.recv(assigned).await;
        assert!(spec.source.is_none(), "[{}]", backend.name());
        assert_eq!(spec.paths, ["/etc"]);
        finish(pool, &files_job).await;

        // The forged capability yields exactly the job of A's own policy.
        let vm_job = run(&api, "vms-a").await;
        let spec = agent.recv(assigned).await;
        assert_eq!(spec.policy_name, "vms-a");
        assert!(spec.paths.is_empty());
        let selector = spec.source.expect("the selector of the policy");
        assert_eq!(selector.r#type, "proxmox-vm");
        assert_eq!(selector.vmids, [101]);
        assert!(selector.pool.is_empty() && selector.tags.is_empty());
        assert!(
            selector.databases.is_empty() && selector.exclude_databases.is_empty(),
            "[{}]",
            backend.name()
        );
        finish(pool, &vm_job).await;

        // A claim of "nothing here", made after the job was queued, only
        // turns the job into a refusal.
        drop(agent);
        let offline = wait_until(Duration::from_secs(10), || async {
            !ts.state().registry.is_online(&a.id)
        })
        .await;
        assert!(offline, "[{}]", backend.name());
        let refused = run(&api, "vms-a").await;
        let nothing = SourceFacts {
            age_ms: 0,
            ..facts(Vec::new())
        };
        sources::record(ts.state(), &a.id, &nothing, &mut None, now_ms())
            .await
            .unwrap();
        let _agent = Fake::connect_with(&ts, &a, &[VM]).await;
        wait_job(pool, &refused, "failed").await;
        assert_eq!(
            job_column(pool, &refused, "error_code").await.as_deref(),
            Some("source_not_found")
        );

        // B never connected: its job stayed queued, and no job of A names an instance.
        let b_jobs = sqlx::query("SELECT state FROM jobs WHERE host_id = $1")
            .bind(&b.id)
            .fetch_all(pool)
            .await
            .unwrap();
        assert!(b_jobs.is_empty(), "[{}]", backend.name());
        let a_jobs = sqlx::query("SELECT source_ref FROM jobs WHERE host_id = $1")
            .bind(&a.id)
            .fetch_all(pool)
            .await
            .unwrap();
        assert_eq!(a_jobs.len(), 3);
        for row in a_jobs {
            assert_eq!(row.try_get::<String, _>(0).unwrap(), "");
        }
        assert_eq!(rights(pool).await, before, "[{}]", backend.name());
        ts.stop().await;
    }
}

fn bloated_guests() -> Vec<ProxmoxGuest> {
    (0..5000u32)
        .map(|i| ProxmoxGuest {
            kind: GuestKind::Vm as i32,
            vmid: 100 + i,
            name: "n".repeat(100),
            uuid: format!("{i:08x}-1111-4111-8111-111111111111"),
            pool: "p".repeat(100),
            tags: (0..100).map(|t| format!("t{t:02}")).collect(),
        })
        .collect()
}

fn bloated_servers() -> Vec<DatabaseServer> {
    (0..100u32)
        .map(|i| DatabaseServer {
            kind: DatabaseKind::Postgres as i32,
            port: 5432 + i,
            version: "v".repeat(100),
            server_id: format!("{}", 7000 + i),
            ..Default::default()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_source_facts_are_bounded() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let pool = &ts.state().db.pool;
        let a = enroll(&ts, &api, &repo, "pve-a").await;
        let b = enroll(&ts, &api, &repo, "pve-b").await;
        let mut agent = Fake::connect(&ts, &a, vec![]).await;

        let mut big = facts(bloated_guests());
        big.databases = bloated_servers();
        heartbeat(&agent, big).await;
        wait_stored(pool, &a.id, |d| d.truncated).await;
        let dropped: Discovery = serde_json::from_str(&stored(pool, &a.id).await.unwrap()).unwrap();
        assert!(
            dropped.proxmox.is_none() && dropped.servers.is_empty(),
            "[{}] a list above the size limit is not stored",
            backend.name()
        );

        let mut cut: Vec<ProxmoxGuest> = bloated_guests()
            .into_iter()
            .take(MAX_GUESTS + 1)
            .map(|g| ProxmoxGuest {
                name: "n".repeat(MAX_NAME_BYTES + 1),
                pool: String::new(),
                tags: Vec::new(),
                ..g
            })
            .collect();
        cut[0].pool = "p".repeat(100);
        cut[0].tags = (0..100).map(|t| format!("t{t:02}")).collect();
        let mut cut = facts(cut);
        cut.databases = bloated_servers();
        let c = enroll(&ts, &api, &repo, "pve-c").await;
        let third = Fake::connect(&ts, &c, vec![]).await;
        heartbeat(&third, cut).await;
        wait_stored(pool, &c.id, |d| d.proxmox.is_some()).await;
        drop(third);
        let kept: Discovery = serde_json::from_str(&stored(pool, &c.id).await.unwrap()).unwrap();
        assert!(kept.truncated);
        let guests = &kept.proxmox.as_ref().unwrap().guests;
        assert_eq!(guests.len(), MAX_GUESTS, "[{}]", backend.name());
        for guest in guests {
            assert!(guest.tags.len() <= MAX_GUEST_TAGS);
            assert!(guest.name.as_ref().unwrap().len() <= MAX_NAME_BYTES);
            assert!(
                guest
                    .pool
                    .as_ref()
                    .is_none_or(|p| p.len() <= MAX_NAME_BYTES)
            );
        }
        assert_eq!(kept.servers.len(), MAX_SERVERS);
        assert!(kept.servers.iter().all(|s| {
            s.databases
                .as_ref()
                .is_none_or(|d| d.len() <= MAX_DATABASES)
        }));
        let before = stored(pool, &a.id).await;

        // A message above the transport limit ends the session and changes nothing.
        let mut huge = facts(vec![guest(101, SHARED_UUID)]);
        huge.proxmox.as_mut().unwrap().cluster = "c".repeat(5 << 20);
        heartbeat(&agent, huge).await;
        let ended = tokio::time::timeout(Duration::from_secs(15), async {
            while let Ok(Some(_)) = agent.rx.message().await {}
        })
        .await;
        assert!(
            ended.is_ok(),
            "[{}] the session stayed open",
            backend.name()
        );
        assert_eq!(stored(pool, &a.id).await, before, "[{}]", backend.name());

        // The server still answers REST and other agents.
        assert_eq!(api.get("/api/v1/hosts").await.status(), 200);
        let other = Fake::connect(&ts, &b, vec![]).await;
        heartbeat(&other, facts(vec![guest(102, SHARED_UUID)])).await;
        wait_stored(pool, &b.id, |d| d.proxmox.is_some()).await;
        drop(other);
        ts.stop().await;
    }
}
