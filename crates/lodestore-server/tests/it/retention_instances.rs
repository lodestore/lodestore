use crate::common;

use std::collections::BTreeMap;

use common::{Backend, TestServer, backends};
use lodestore_policy::retention::Rule;
use lodestore_policy::{BackupPolicy, parse_yaml};
use lodestore_server::catalog::retention::{PairOutcome, plan_policy, run_for_pair};
use lodestore_server::db::now_ms;
use sqlx::Row;

const DAY_MS: i64 = 24 * 3600 * 1000;
const STATS: &str = r#"{"files_new":0,"files_changed":0,"files_unchanged":0,"dirs":0,"bytes_total":0,"bytes_read":0,"bytes_added":0}"#;

const HOUR_MS: i64 = 3600 * 1000;
const T0: i64 = 1_780_315_200_000;

type Planned = (String, String, bool, Vec<Rule>);

struct Seed<'a> {
    host: &'a str,
    source_ref: &'a str,
    received: i64,
    origin: Option<&'a str>,
    stdin: Option<&'a str>,
}

struct Instances {
    ts: TestServer,
    repo: String,
    next: u64,
}

fn guest(n: usize) -> String {
    format!("proxmox-vm/pve1/00000000-0000-4000-8000-{n:012}")
}

fn source_type(source_ref: &str) -> &str {
    source_ref
        .split('/')
        .next()
        .filter(|t| !t.is_empty())
        .unwrap_or("files")
}

fn policy(retention: &str) -> BackupPolicy {
    let yaml = format!(
        "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: inst\n\
         spec:\n  repository: main\n  selector:\n    matchLabels:\n      env: test\n  \
         paths:\n    - /data\n  retention:\n{retention}"
    );
    parse_yaml(&yaml).unwrap()
}

impl Instances {
    async fn start(backend: &Backend, hosts: &[&str]) -> Instances {
        let ts = TestServer::start(backend).await;
        let repo = ts.create_local_repository("main").await;
        for host in hosts {
            common::seed_agent(&ts, &repo, host, "[]").await;
        }
        Instances { ts, repo, next: 0 }
    }

    async fn seed(&mut self, row: Seed<'_>) -> String {
        self.next += 1;
        let id = format!("{:064x}", self.next);
        let label = (!row.source_ref.is_empty()).then_some("VM 101 \u{b7} web");
        sqlx::query(
            "INSERT INTO snapshots (id, repository_id, host_id, hostname, time_ms, paths_json, \
             tags_json, policy_name, stdin_name, job_id, incomplete, errors_total, warnings_total, \
             stats_json, discovered_at, received_at, received_exact, verified_at, verified_by, \
             origin, source_type, source_ref, source_label) \
             VALUES ($1, $2, $3, 'inserted', $4, '[]', '[]', 'inst', $5, 'seeded', 0, 0, 0, $6, \
             $4, $4, 1, $4, 'server', $7, $8, $9, $10)",
        )
        .bind(&id)
        .bind(&self.repo)
        .bind(row.host)
        .bind(row.received)
        .bind(row.stdin)
        .bind(STATS)
        .bind(row.origin)
        .bind(source_type(row.source_ref))
        .bind(row.source_ref)
        .bind(label)
        .execute(&self.ts.state().db.pool)
        .await
        .unwrap();
        id
    }

    async fn run(&self, policy: &BackupPolicy, host: &str) -> PairOutcome {
        run_for_pair(self.ts.state(), policy, host, "no-such-job")
            .await
            .unwrap()
    }

    async fn plan(&self, policy: &BackupPolicy) -> BTreeMap<String, Vec<Planned>> {
        let rules = policy.spec.retention.clone().unwrap();
        let plan = plan_policy(self.ts.state(), policy, &rules).await.unwrap();
        plan.hosts
            .into_iter()
            .map(|h| {
                let items = h
                    .items
                    .into_iter()
                    .map(|i| (i.id, i.source_ref, i.keep, i.reasons))
                    .collect();
                (h.host_id, items)
            })
            .collect()
    }

    async fn stdin_left(&self, host: &str, name: &str) -> Vec<i64> {
        sqlx::query(
            "SELECT received_at FROM snapshots WHERE host_id = $1 AND stdin_name = $2 \
             ORDER BY received_at DESC",
        )
        .bind(host)
        .bind(name)
        .fetch_all(&self.ts.state().db.pool)
        .await
        .unwrap()
        .iter()
        .map(|r| r.try_get("received_at").unwrap())
        .collect()
    }

    async fn left(&self, host: &str) -> BTreeMap<String, Vec<i64>> {
        let rows = sqlx::query(
            "SELECT source_ref, received_at FROM snapshots WHERE host_id = $1 \
             ORDER BY received_at DESC",
        )
        .bind(host)
        .fetch_all(&self.ts.state().db.pool)
        .await
        .unwrap();
        let mut by_ref: BTreeMap<String, Vec<i64>> = BTreeMap::new();
        for r in rows {
            by_ref
                .entry(r.try_get("source_ref").unwrap())
                .or_default()
                .push(r.try_get("received_at").unwrap());
        }
        by_ref
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_instances_40_guests_keep_their_own_history() {
    for backend in backends() {
        let mut inst = Instances::start(&backend, &["h1"]).await;
        let base = now_ms() - 200 * DAY_MS;
        let mut seeded: BTreeMap<String, Vec<i64>> = BTreeMap::new();
        for n in 0..40 {
            let key = guest(n);
            let count = if n == 7 { 13 } else { 3 };
            for k in 0..count {
                let received = base + k * DAY_MS + n as i64 * 1000;
                inst.seed(Seed {
                    host: "h1",
                    source_ref: &key,
                    received,
                    origin: Some("schedule"),
                    stdin: None,
                })
                .await;
                seeded.entry(key.clone()).or_default().push(received);
            }
        }

        inst.run(&policy("    keepLast: 2\n"), "h1").await;

        let left = inst.left("h1").await;
        let name = backend.name();
        assert_eq!(left.values().map(Vec::len).sum::<usize>(), 80, "{name}");
        for (key, mut times) in seeded {
            times.sort_by(|a, b| b.cmp(a));
            times.truncate(2);
            assert_eq!(left[&key], times, "{name} {key}");
        }
        inst.ts.stop().await;
    }
}

impl Instances {
    async fn put(&mut self, host: &str, source_ref: &str, received: i64) {
        self.seed(Seed {
            host,
            source_ref,
            received,
            origin: Some("schedule"),
            stdin: None,
        })
        .await;
    }

    async fn put_days(&mut self, host: &str, source_ref: &str, count: i64) {
        for k in 0..count {
            self.put(host, source_ref, T0 + k * DAY_MS).await;
        }
    }

    async fn put_stream(&mut self, name: &str, received: i64) {
        self.seed(Seed {
            host: "h1",
            source_ref: "",
            received,
            origin: Some("stdin"),
            stdin: Some(name),
        })
        .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_instances_files_instance_is_the_host() {
    for backend in backends() {
        let mut inst = Instances::start(&backend, &["host-a", "host-b"]).await;
        inst.put_days("host-a", "", 5).await;
        inst.put_days("host-b", "", 5).await;
        let policy = policy("    keepLast: 2\n");
        let name = backend.name();

        let both = inst.plan(&policy).await;
        for host in ["host-a", "host-b"] {
            assert_eq!(both[host].len(), 5, "{name} {host}");
            assert_eq!(
                both[host].iter().filter(|i| i.2).count(),
                2,
                "{name} {host}"
            );
        }
        sqlx::query("DELETE FROM snapshots WHERE host_id = 'host-b'")
            .execute(&inst.ts.state().db.pool)
            .await
            .unwrap();
        let alone = inst.plan(&policy).await;
        assert_eq!(both["host-a"], alone["host-a"], "{name}");

        inst.put_days("host-b", "", 5).await;
        inst.run(&policy, "host-a").await;
        assert_eq!(inst.left("host-a").await[""].len(), 2, "{name}");
        assert_eq!(inst.left("host-b").await[""].len(), 5, "{name}");
        inst.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_instances_reused_vmid_is_a_new_instance() {
    for backend in backends() {
        let mut inst = Instances::start(&backend, &["h1"]).await;
        let old = "proxmox-vm/pve1/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let new = "proxmox-vm/pve1/bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        inst.put_days("h1", old, 4).await;
        inst.put_days("h1", new, 4).await;

        inst.run(&policy("    keepLast: 1\n"), "h1").await;

        let left = inst.left("h1").await;
        let name = backend.name();
        assert_eq!(left.values().map(Vec::len).sum::<usize>(), 4, "{name}");
        assert_eq!(left[old], [T0 + 3 * DAY_MS, T0 + 2 * DAY_MS], "{name}");
        assert_eq!(left[new], [T0 + 3 * DAY_MS, T0 + 2 * DAY_MS], "{name}");
        inst.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_instances_recreated_database_is_a_new_instance() {
    for backend in backends() {
        let mut inst = Instances::start(&backend, &["h1"]).await;
        let old = "postgres/7300000000000000001/16384";
        let new = "postgres/7300000000000000001/16999";
        inst.put_days("h1", old, 4).await;
        inst.put_days("h1", new, 4).await;

        inst.run(&policy("    keepLast: 1\n"), "h1").await;

        let left = inst.left("h1").await;
        let name = backend.name();
        assert_eq!(left.values().map(Vec::len).sum::<usize>(), 4, "{name}");
        assert_eq!(left[old], [T0 + 3 * DAY_MS, T0 + 2 * DAY_MS], "{name}");
        assert_eq!(left[new], [T0 + 3 * DAY_MS, T0 + 2 * DAY_MS], "{name}");
        inst.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_instances_gone_instance_keeps_the_floor() {
    for backend in backends() {
        let mut inst = Instances::start(&backend, &["h1"]).await;
        let (gone, active) = (guest(1), guest(2));
        for k in 0..5 {
            inst.put("h1", &gone, T0 + k * HOUR_MS).await;
        }
        let policy = policy("    keepLast: 1\n");

        for n in 0..12 {
            inst.put("h1", &active, T0 + 7 * DAY_MS + n * HOUR_MS).await;
            inst.run(&policy, "h1").await;
        }

        let left = inst.left("h1").await;
        let name = backend.name();
        assert_eq!(left[&gone], [T0 + 4 * HOUR_MS, T0 + 3 * HOUR_MS], "{name}");
        assert_eq!(left[&active].len(), 2, "{name}");
        let plan = inst.plan(&policy).await;
        let items: Vec<&Planned> = plan["h1"].iter().filter(|i| i.1 == gone).collect();
        assert_eq!(items.len(), 2, "{name}");
        for item in items {
            assert!(item.2 && item.3.contains(&Rule::Floor), "{name} {item:?}");
        }
        inst.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_instances_stdin_streams_have_their_own_pools() {
    for backend in backends() {
        let mut inst = Instances::start(&backend, &["h1"]).await;
        let weekly = |k: i64| T0 - 30 * 60_000 - k * 7 * DAY_MS;
        let hourly = |h: i64| T0 - h * HOUR_MS;
        for k in 0..4 {
            inst.put_stream("etcd.snap", weekly(k)).await;
        }
        for h in 0..72 {
            inst.put_stream("pg.dump", hourly(h)).await;
        }

        let rules = "    keepDaily: 2\n    includeStdin: true\n";
        inst.run(&policy(rules), "h1").await;

        let name = backend.name();
        let etcd = inst.stdin_left("h1", "etcd.snap").await;
        assert_eq!(etcd, [weekly(0), weekly(1)], "{name}");
        let dump = inst.stdin_left("h1", "pg.dump").await;
        assert_eq!(dump, [hourly(0), hourly(1), hourly(13)], "{name}");
        inst.ts.stop().await;
    }
}
