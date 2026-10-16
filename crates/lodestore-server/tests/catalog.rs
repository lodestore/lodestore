mod common;

use common::{Api, TestServer, backends};
use lodestore_agent::{EnrollArgs, NetOptions};
use lodestore_format::crypto::Entropy;
use lodestore_format::index::hex32;
use lodestore_format::repo::Repository;
use lodestore_format::snapshot::{open_snapshot, seal_snapshot, snapshot_object_key};
use lodestore_format::store::{LocalFsStore, put_named};
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_server::catalog::{JobLink, ingest_snapshot};
use serde_json::{Value, json};

const PASSWORD: &[u8] = b"test-password";

async fn enroll(ts: &TestServer, api: &Api, repo: &str, hostname: &str, dir: &str) -> String {
    let token = api.join_token(repo, json!([])).await;
    lodestore_agent::enroll(EnrollArgs {
        token,
        paths: ts.agent_paths(dir),
        hostname: Some(hostname.to_string()),
        net: NetOptions::default(),
    })
    .await
    .expect("enroll");
    api.hosts()
        .await
        .iter()
        .filter(|h| h["hostname"] == hostname)
        .map(|h| h["id"].as_str().unwrap().to_string())
        .next_back()
        .expect("the enrolled host")
}

async fn write_snapshot(ts: &TestServer, name: &str, hostname: &str, tags: &[&str]) -> String {
    let store = LocalFsStore::new(ts.local_repo_root(name));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    let mut writer = Writer::new(&store, keys, repo.config()).await.unwrap();
    let data = ts.dir.path().join(format!("data-{hostname}"));
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("file.txt"), format!("content of {hostname}")).unwrap();
    let outcome = writer
        .backup(
            &[data],
            BackupOptions {
                hostname: Some(hostname.as_bytes().to_vec()),
                tags: tags.iter().map(|t| t.to_string()).collect(),
                ..BackupOptions::default()
            },
            &|_| {},
        )
        .await
        .expect("backup");
    hex32(&outcome.snapshot_id)
}

async fn forge_snapshot(
    ts: &TestServer,
    name: &str,
    id: &str,
    edit: impl FnOnce(&mut lodestore_format::pb::Snapshot),
) -> String {
    let store = LocalFsStore::new(ts.local_repo_root(name));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let keys = repo.read_keys();
    let id = lodestore_format::index::parse_hex32(id).unwrap();
    let bytes = std::fs::read(ts.local_repo_root(name).join(snapshot_object_key(&id))).unwrap();
    let mut snapshot = open_snapshot(keys.hpke_private_key(), keys.repo_id(), &id, &bytes).unwrap();
    edit(&mut snapshot);
    let (forged, bytes) = seal_snapshot(
        &snapshot,
        keys.hpke_public_key(),
        keys.repo_id(),
        &mut Entropy::os(),
    )
    .unwrap();
    put_named(&store, &snapshot_object_key(&forged), bytes)
        .await
        .unwrap();
    hex32(&forged)
}

async fn sync(api: &Api, repo: &str) -> Value {
    let response = api
        .post(
            &format!("/api/v1/repositories/{repo}/catalog/sync"),
            &json!({}),
        )
        .await;
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

async fn host_snapshots(api: &Api, host: &str) -> Vec<Value> {
    let response = api.get(&format!("/api/v1/hosts/{host}/snapshots")).await;
    assert_eq!(response.status(), 200);
    let page: Value = response.json().await.unwrap();
    page["items"].as_array().unwrap().clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalog_sync_finds_direct_snapshot() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a", "a").await;
        let id = write_snapshot(&ts, "main", "host-a", &[]).await;

        assert!(host_snapshots(&api, &host).await.is_empty());
        let first = sync(&api, &repo).await;
        assert_eq!(first["added"], 1, "[{}]", backend.name());
        assert_eq!(first["total"], 1);

        let items = host_snapshots(&api, &host).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], id);
        assert_eq!(items[0]["hostname"], "host-a");
        assert!(
            items[0]["paths"][0]
                .as_str()
                .unwrap()
                .contains("data-host-a")
        );
        assert_eq!(items[0]["stats"]["files_new"], 1);

        assert_eq!(sync(&api, &repo).await["added"], 0);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalog_binds_by_agent_id_claim() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let a = enroll(&ts, &api, &repo, "host-a", "a").await;
        let b = enroll(&ts, &api, &repo, "host-b", "b").await;
        let original = write_snapshot(&ts, "main", "host-a", &[]).await;
        let claim = b.clone();
        forge_snapshot(&ts, "main", &original, |s| {
            s.agent_id = claim.into_bytes();
        })
        .await;
        std::fs::remove_file(
            ts.local_repo_root("main")
                .join(format!("snapshots/{original}")),
        )
        .unwrap();

        assert_eq!(sync(&api, &repo).await["added"], 1);
        assert_eq!(
            host_snapshots(&api, &b).await.len(),
            1,
            "[{}]",
            backend.name()
        );
        assert!(host_snapshots(&api, &a).await.is_empty());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalog_ambiguous_hostname_stays_unbound() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let first = enroll(&ts, &api, &repo, "dup", "one").await;
        let second = enroll(&ts, &api, &repo, "dup", "two").await;
        assert_ne!(first, second);
        write_snapshot(&ts, "main", "dup", &[]).await;

        assert_eq!(sync(&api, &repo).await["added"], 1);
        assert!(host_snapshots(&api, &first).await.is_empty());
        assert!(host_snapshots(&api, &second).await.is_empty());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalog_ingest_links_job() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let a = enroll(&ts, &api, &repo, "host-a", "a").await;
        let b = enroll(&ts, &api, &repo, "host-b", "b").await;
        let id = write_snapshot(&ts, "main", "host-a", &[]).await;

        let link = || JobLink {
            job_id: "job-1",
            host_id: &b,
        };
        let state = ts.state();
        assert!(
            ingest_snapshot(state, &repo, &id, Some(link()))
                .await
                .unwrap()
        );
        assert!(
            !ingest_snapshot(state, &repo, &id, Some(link()))
                .await
                .unwrap()
        );

        let items = host_snapshots(&api, &b).await;
        assert_eq!(items.len(), 1, "[{}]", backend.name());
        assert_eq!(items[0]["job_id"], "job-1");
        assert!(host_snapshots(&api, &a).await.is_empty());
        assert_eq!(sync(&api, &repo).await["added"], 0);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalog_ingest_cannot_rebind_a_known_snapshot() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let a = enroll(&ts, &api, &repo, "host-a", "a").await;
        let b = enroll(&ts, &api, &repo, "host-b", "b").await;
        let id = write_snapshot(&ts, "main", "host-a", &[]).await;
        let state = ts.state();

        assert_eq!(sync(&api, &repo).await["added"], 1);
        assert_eq!(host_snapshots(&api, &a).await.len(), 1);

        let stolen = ingest_snapshot(
            state,
            &repo,
            &id,
            Some(JobLink {
                job_id: "job-b",
                host_id: &b,
            }),
        )
        .await;
        assert!(stolen.is_err(), "[{}]", backend.name());
        assert_eq!(host_snapshots(&api, &a).await.len(), 1);
        assert!(host_snapshots(&api, &b).await.is_empty());

        ingest_snapshot(
            state,
            &repo,
            &id,
            Some(JobLink {
                job_id: "job-a",
                host_id: &a,
            }),
        )
        .await
        .expect("the owning host links its job");
        let again = ingest_snapshot(
            state,
            &repo,
            &id.to_uppercase(),
            Some(JobLink {
                job_id: "job-other",
                host_id: &a,
            }),
        )
        .await;
        assert!(again.is_err());
        let items = host_snapshots(&api, &a).await;
        assert_eq!(items[0]["job_id"], "job-a");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalog_ingest_is_scoped_to_the_repository() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let other = ts.create_local_repository("other").await;
        let a = enroll(&ts, &api, &repo, "host-a", "a").await;
        let id = write_snapshot(&ts, "main", "host-a", &[]).await;
        sync(&api, &repo).await;

        let result = ingest_snapshot(
            ts.state(),
            &other,
            &id,
            Some(JobLink {
                job_id: "job-x",
                host_id: &a,
            }),
        )
        .await;
        assert!(result.is_err(), "[{}]", backend.name());
        assert!(host_snapshots(&api, &a).await[0]["job_id"].is_null());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalog_sync_survives_an_extreme_timestamp() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a", "a").await;
        let original = write_snapshot(&ts, "main", "host-a", &[]).await;
        forge_snapshot(&ts, "main", &original, |s| {
            s.time = Some(lodestore_format::pb::Timestamp {
                seconds: i64::MAX,
                nanos: 0,
            });
        })
        .await;

        let report = sync(&api, &repo).await;
        assert_eq!(report["added"], 2, "[{}]", backend.name());
        assert_eq!(host_snapshots(&api, &host).await.len(), 2);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalog_tags_policy_and_stdin() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a", "a").await;
        write_snapshot(
            &ts,
            "main",
            "host-a",
            &["policy=etc", "stdin=app.dump", "other"],
        )
        .await;

        sync(&api, &repo).await;
        let items = host_snapshots(&api, &host).await;
        assert_eq!(items[0]["policy_name"], "etc", "[{}]", backend.name());
        assert_eq!(items[0]["stdin_name"], "app.dump");
        assert_eq!(items[0]["tags"].as_array().unwrap().len(), 3);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalog_skips_unreadable_objects() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a", "a").await;
        write_snapshot(&ts, "main", "host-a", &[]).await;
        let dir = ts.local_repo_root("main").join("snapshots");
        std::fs::write(dir.join("0".repeat(64)), b"not a snapshot").unwrap();
        std::fs::write(dir.join("junk"), b"not a snapshot").unwrap();

        let report = sync(&api, &repo).await;
        assert_eq!(report["added"], 1, "[{}]", backend.name());
        assert_eq!(report["skipped"], 2);
        assert_eq!(host_snapshots(&api, &host).await.len(), 1);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalog_unknown_ids_404() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let response = api
            .post("/api/v1/repositories/nope/catalog/sync", &json!({}))
            .await;
        assert_eq!(response.status(), 404, "[{}]", backend.name());
        assert_eq!(api.get("/api/v1/hosts/nope/snapshots").await.status(), 404);
        ts.stop().await;
    }
}
