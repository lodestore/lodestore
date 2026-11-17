use crate::common;

use std::collections::BTreeMap;

use common::{Backend, TestServer, backends};
use lodestore_policy::{BackupPolicy, parse_yaml};
use lodestore_server::catalog::retention::{PairOutcome, run_for_pair};
use lodestore_server::db::now_ms;
use sqlx::Row;

const DAY_MS: i64 = 24 * 3600 * 1000;
const STATS: &str = r#"{"files_new":0,"files_changed":0,"files_unchanged":0,"dirs":0,"bytes_total":0,"bytes_read":0,"bytes_added":0}"#;

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
