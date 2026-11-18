use crate::common;

use common::{Api, TestServer, backends};
use lodestore_server::db::now_ms;
use serde_json::Value;

const VM_A: &str = "proxmox-vm/pve1/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const VM_B: &str = "proxmox-vm/pve1/bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const STATS: &str = r#"{"files_new":0,"files_changed":0,"files_unchanged":0,"dirs":0,"bytes_total":0,"bytes_read":0,"bytes_added":0}"#;
const WAL: &str = "postgres-wal/7300000000000000001";

struct Row<'a> {
    id: char,
    host: &'a str,
    source_type: &'a str,
    source_ref: &'a str,
    label: Option<&'a str>,
    received: i64,
}

fn files(id: char, host: &str, received: i64) -> Row<'_> {
    Row {
        id,
        host,
        source_type: "files",
        source_ref: "",
        label: None,
        received,
    }
}

fn guest<'a>(id: char, host: &'a str, key: &'a str, received: i64) -> Row<'a> {
    Row {
        id,
        host,
        source_type: key.split('/').next().unwrap(),
        source_ref: key,
        label: Some("VM 101 \u{b7} web"),
        received,
    }
}

fn sid(id: char) -> String {
    id.to_string().repeat(64)
}

async fn seed(ts: &TestServer, repo: &str, row: Row<'_>) {
    sqlx::query(
        "INSERT INTO snapshots (id, repository_id, host_id, hostname, time_ms, paths_json, \
         tags_json, incomplete, errors_total, warnings_total, stats_json, discovered_at, \
         received_at, received_exact, verified_at, verified_by, source_type, source_ref, \
         source_label) \
         VALUES ($1, $2, $3, 'inserted', $4, '[]', '[]', 0, 0, 0, $8, $4, $4, 1, $4, 'server', \
         $5, $6, $7)",
    )
    .bind(sid(row.id))
    .bind(repo)
    .bind(row.host)
    .bind(row.received)
    .bind(row.source_type)
    .bind(row.source_ref)
    .bind(row.label)
    .bind(STATS)
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
}

async fn json(api: &Api, path: &str) -> Value {
    let response = api.get(path).await;
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, 200, "{path}: {body}");
    serde_json::from_str(&body).unwrap()
}

fn ids(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap()[..1].to_string())
        .collect()
}

async fn start(backend: &common::Backend, hosts: &[&str]) -> (TestServer, Api, String) {
    let ts = TestServer::start(backend).await;
    let repo = ts.create_local_repository("main").await;
    for host in hosts {
        common::seed_agent(&ts, &repo, host, "[]").await;
    }
    let api = ts.api().await;
    (ts, api, repo)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn source_filter_host_snapshots_by_type() {
    for backend in backends() {
        let (ts, api, repo) = start(&backend, &["h1"]).await;
        let t = now_ms() - 10_000;
        for (n, id) in ['1', '2', '3'].into_iter().enumerate() {
            seed(&ts, &repo, files(id, "h1", t + n as i64)).await;
        }
        seed(&ts, &repo, guest('4', "h1", VM_A, t + 10)).await;
        seed(&ts, &repo, guest('5', "h1", VM_A, t + 11)).await;

        let all = json(&api, "/api/v1/hosts/h1/snapshots").await;
        assert_eq!(all["total"], 5);
        assert_eq!(
            all["source_counts"],
            serde_json::json!([
                {"source_type": "files", "count": 3},
                {"source_type": "proxmox-vm", "count": 2},
            ])
        );
        for item in all["items"].as_array().unwrap() {
            if item["source_type"] == "files" {
                assert!(item["source_label"].is_null());
            } else {
                assert_eq!(item["source_label"], "VM 101 \u{b7} web");
            }
        }

        let vm = json(&api, "/api/v1/hosts/h1/snapshots?source_type=proxmox-vm").await;
        assert_eq!(vm["total"], 2);
        assert_eq!(ids(&vm), ["5", "4"]);
        assert_eq!(vm["source_counts"], all["source_counts"]);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn source_filter_unknown_type_is_422() {
    for backend in backends() {
        let (ts, api, _) = start(&backend, &["h1"]).await;
        for path in [
            "/api/v1/hosts/h1/snapshots?source_type=oracle",
            "/api/v1/restore/snapshots?host=h1&source_type=oracle",
        ] {
            assert_eq!(api.get(path).await.status(), 422, "{path}");
        }
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn source_filter_restore_list_and_latest_by_type() {
    for backend in backends() {
        let (ts, api, repo) = start(&backend, &["h1"]).await;
        let t = now_ms() - 10_000;
        seed(&ts, &repo, files('1', "h1", t)).await;
        seed(&ts, &repo, files('2', "h1", t + 1)).await;
        seed(&ts, &repo, guest('3', "h1", VM_A, t + 2)).await;
        seed(&ts, &repo, guest('4', "h1", VM_A, t + 3)).await;
        seed(&ts, &repo, guest('5', "h1", WAL, t + 4)).await;

        let by_default = json(&api, "/api/v1/restore/snapshots?host=h1").await;
        assert_eq!(by_default["latest_verified_id"], sid('2'));
        assert_eq!(by_default["total"], 5);
        let vm = json(
            &api,
            "/api/v1/restore/snapshots?host=h1&source_type=proxmox-vm",
        )
        .await;
        assert_eq!(vm["latest_verified_id"], sid('4'));
        assert_eq!(vm["total"], 2);
        assert_eq!(ids(&vm), ["4", "3"]);
        let wal = json(
            &api,
            "/api/v1/restore/snapshots?host=h1&source_type=postgres-wal",
        )
        .await;
        assert!(wal["latest_verified_id"].is_null());
        assert_eq!(ids(&wal), ["5"]);

        let paged = json(
            &api,
            &format!(
                "/api/v1/restore/snapshots?host=h1&source_type=proxmox-vm&limit=1&before={}.{}",
                t + 3,
                sid('4')
            ),
        )
        .await;
        assert_eq!(ids(&paged), ["3"]);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn source_filter_latest_of_one_instance() {
    for backend in backends() {
        let (ts, api, repo) = start(&backend, &["h1", "h2"]).await;
        let t = now_ms() - 20_000;
        for (n, (id, key)) in [
            ('1', VM_A),
            ('2', VM_B),
            ('3', VM_A),
            ('4', VM_B),
            ('5', VM_B),
            ('6', VM_A),
        ]
        .into_iter()
        .enumerate()
        {
            seed(&ts, &repo, guest(id, "h1", key, t + n as i64)).await;
        }
        seed(&ts, &repo, guest('7', "h2", VM_A, t + 100)).await;
        seed(&ts, &repo, guest('8', "h1", WAL, t + 200)).await;
        let list =
            |host: &str, query: &str| format!("/api/v1/restore/snapshots?host={host}{query}");
        let enc = |key: &str| key.replace('/', "%2F");

        let a = json(&api, &list("h1", &format!("&source={}", enc(VM_A)))).await;
        assert_eq!(ids(&a), ["6", "3", "1"]);
        assert_eq!(a["total"], 3);
        assert_eq!(a["latest_verified_id"], sid('6'));
        let b = json(
            &api,
            &list(
                "h1",
                &format!("&source={}&source_type=proxmox-vm", enc(VM_B)),
            ),
        )
        .await;
        assert_eq!(b["latest_verified_id"], sid('5'));
        let other_host = json(&api, &list("h2", &format!("&source={}", enc(VM_A)))).await;
        assert_eq!(ids(&other_host), ["7"]);
        assert_eq!(other_host["latest_verified_id"], sid('7'));
        let wal = json(&api, &list("h1", &format!("&source={}", enc(WAL)))).await;
        assert_eq!(ids(&wal), ["8"]);
        assert!(wal["latest_verified_id"].is_null());

        for query in [
            format!("&source={}&source_type=postgres", enc(VM_A)),
            "&source=proxmox-vm%2Fx".to_string(),
        ] {
            assert_eq!(api.get(&list("h1", &query)).await.status(), 422, "{query}");
        }
        ts.stop().await;
    }
}
