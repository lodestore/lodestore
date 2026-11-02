mod common;

use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Api, TestServer, backends, wait_until};
use data_encoding::BASE64URL_NOPAD;
use lodestore_format::index::hex32;
use lodestore_format::keys::MasterKey;
use lodestore_format::repo::Repository;
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_server::auth::session_janitor;
use lodestore_server::catalog::ingest_snapshot;
use lodestore_server::db::now_ms;
use lodestore_server::repos::db::{AccessKey, NewRepository, RepoLocation, create_repository};
use serde_json::{Value, json};
use sqlx::Row;
use tokio_util::sync::CancellationToken;

const PASSWORD: &[u8] = b"test-password";
const MIB: usize = 1 << 20;

fn b64(bytes: &[u8]) -> String {
    BASE64URL_NOPAD.encode(bytes)
}

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

struct Seeded {
    repo: String,
    snapshot: String,
    base: String,
}

impl Seeded {
    fn item(&self, below: &str) -> String {
        b64(format!("{}/{below}", self.base).as_bytes())
    }
}

async fn seed(ts: &TestServer, big: usize) -> Seeded {
    let repo = ts.create_local_repository("main").await;
    let store = LocalFsStore::new(ts.local_repo_root("main"));
    let opened = Repository::open(&store, PASSWORD).await.unwrap();
    let keys = opened.read_keys().write_keys(opened.config_hash());
    let mut writer = Writer::new(&store, keys, opened.config()).await.unwrap();
    let data = ts.dir.path().join("tree");
    std::fs::create_dir_all(data.join("docs/sub")).unwrap();
    std::fs::write(data.join("docs/a.txt"), b"alpha").unwrap();
    std::fs::write(data.join("docs/sub/c.txt"), b"charlie").unwrap();
    std::fs::write(data.join("b.txt"), b"bravo").unwrap();
    if big > 0 {
        std::fs::write(data.join("big.bin"), noise(big, 7)).unwrap();
    }
    let outcome = writer
        .backup(
            std::slice::from_ref(&data),
            BackupOptions {
                hostname: Some(b"host-a".to_vec()),
                ..BackupOptions::default()
            },
            &|_| {},
        )
        .await
        .expect("backup");
    let snapshot = hex32(&outcome.snapshot_id);
    ingest_snapshot(ts.state(), &repo, &snapshot, None)
        .await
        .expect("ingest");
    Seeded {
        repo,
        snapshot,
        base: data.canonicalize().unwrap().display().to_string(),
    }
}

async fn create(api: &Api, seeded: &Seeded, items: &[&str], format: &str) -> Value {
    let response = api
        .post(
            "/api/v1/downloads",
            &json!({
                "snapshot_id": seeded.snapshot,
                "items": items.iter().map(|i| seeded.item(i)).collect::<Vec<_>>(),
                "format": format,
            }),
        )
        .await;
    assert_eq!(response.status(), 201);
    response.json().await.unwrap()
}

async fn row(ts: &TestServer, id: &str) -> (String, i64, Option<String>, String) {
    let row = sqlx::query(
        "SELECT state, bytes_sent, error_code, started_by FROM downloads WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&ts.state().db.pool)
    .await
    .unwrap();
    (
        row.try_get("state").unwrap(),
        row.try_get("bytes_sent").unwrap(),
        row.try_get("error_code").unwrap(),
        row.try_get("started_by").unwrap(),
    )
}

async fn wait_state(ts: &TestServer, id: &str, want: &str) {
    let reached = wait_until(Duration::from_secs(10), || async {
        row(ts, id).await.0 == want
    })
    .await;
    assert!(
        reached,
        "download {id} is {}, wanted {want}",
        row(ts, id).await.0
    );
}

fn tar_files(tar_bytes: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let mut archive = tar::Archive::new(Cursor::new(tar_bytes));
    let mut out = BTreeMap::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        if entry.header().entry_type().is_file() {
            let name = entry.path().unwrap().display().to_string();
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            out.insert(name, data);
        }
    }
    out
}

fn find<'a>(files: &'a BTreeMap<String, Vec<u8>>, tail: &str) -> Option<&'a Vec<u8>> {
    files
        .iter()
        .find(|(name, _)| name.ends_with(tail))
        .map(|(_, data)| data)
}

fn list_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_tar_gz_streams_selection() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let seeded = seed(&ts, 0).await;
        let created = create(&api, &seeded, &["docs"], "tar_gz").await;
        let id = created["id"].as_str().unwrap();
        let file_name = created["file_name"].as_str().unwrap();
        assert_eq!(
            file_name,
            format!("host-a-{}.tar.gz", &seeded.snapshot[..8])
        );

        let response = api.get(created["url"].as_str().unwrap()).await;
        assert_eq!(response.status(), 200, "[{}]", backend.name());
        assert_eq!(response.headers()["content-type"], "application/gzip");
        assert_eq!(
            response.headers()["content-disposition"],
            format!("attachment; filename=\"{file_name}\"")
        );
        let body = response.bytes().await.unwrap();
        let mut tar_bytes = Vec::new();
        flate2::read::GzDecoder::new(&body[..])
            .read_to_end(&mut tar_bytes)
            .unwrap();
        let files = tar_files(&tar_bytes);
        assert_eq!(files.len(), 2);
        assert_eq!(find(&files, "docs/a.txt").unwrap(), b"alpha");
        assert_eq!(find(&files, "docs/sub/c.txt").unwrap(), b"charlie");

        wait_state(&ts, id, "succeeded").await;
        let (_, sent, code, by) = row(&ts, id).await;
        assert_eq!(sent as usize, body.len());
        assert_eq!(code, None);
        assert_eq!(by, "admin");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_ticket_single_use_and_expires() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let seeded = seed(&ts, 0).await;

        let created = create(&api, &seeded, &["b.txt"], "tar_gz").await;
        let url = created["url"].as_str().unwrap();
        let first = api.get(url).await;
        assert_eq!(first.status(), 200);
        assert_eq!(first.headers()["x-frame-options"], "SAMEORIGIN");
        let other = api.get("/api/v1/auth/session").await;
        assert_eq!(other.headers()["x-frame-options"], "DENY");
        let again = api.get(url).await;
        assert_eq!(again.status(), 404);
        assert_eq!(again.headers()["x-frame-options"], "SAMEORIGIN");
        let csp = again.headers()["content-security-policy"].to_str().unwrap();
        assert!(csp.ends_with("frame-ancestors 'self'"), "{csp}");
        let error: Value = again.json().await.unwrap();
        assert_eq!(
            error["message"],
            "This download link was already used or expired."
        );

        let stale = create(&api, &seeded, &["b.txt"], "zip").await;
        sqlx::query("UPDATE downloads SET created_at = created_at - 61000 WHERE id = $1")
            .bind(stale["id"].as_str().unwrap())
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        assert_eq!(
            api.get(stale["url"].as_str().unwrap()).await.status(),
            404,
            "[{}]",
            backend.name()
        );
        assert_eq!(row(&ts, stale["id"].as_str().unwrap()).await.0, "pending");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_client_drop_marks_interrupted() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let seeded = seed(&ts, 24 * MIB).await;
        let created = create(&api, &seeded, &["big.bin"], "tar_gz").await;
        let id = created["id"].as_str().unwrap();

        let mut response = api.get(created["url"].as_str().unwrap()).await;
        assert_eq!(response.status(), 200);
        assert!(response.chunk().await.unwrap().is_some());
        drop(response);

        wait_state(&ts, id, "interrupted").await;
        assert_eq!(row(&ts, id).await.2.as_deref(), Some("client_closed"));
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_reports_bytes_while_streaming() {
    let backend = backends().remove(0);
    let ts = TestServer::start(&backend).await;
    let api = ts.api().await;
    let seeded = seed(&ts, 24 * MIB).await;
    let created = create(&api, &seeded, &["big.bin"], "zip").await;
    let id = created["id"].as_str().unwrap();

    let mut response = api.get(created["url"].as_str().unwrap()).await;
    assert_eq!(response.status(), 200);
    assert!(response.chunk().await.unwrap().is_some());
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let (state, sent, ..) = row(&ts, id).await;
        if sent > 0 {
            assert_eq!(state, "running", "progress is written before the end");
            break;
        }
        assert!(std::time::Instant::now() < deadline, "no progress written");
        response
            .chunk()
            .await
            .unwrap()
            .expect("more of the archive");
    }
    drop(response);
    wait_state(&ts, id, "interrupted").await;
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_writes_no_temp_files() {
    let ts = TestServer::start(&common::Backend::Sqlite).await;
    let api = ts.api().await;
    let scratch = ts.dir.path().join("scratch-tmp");
    std::fs::create_dir_all(&scratch).unwrap();
    // SAFETY: nextest runs each test in its own process and no other thread reads the environment here.
    unsafe { std::env::set_var("TMPDIR", &scratch) };
    let seeded = seed(&ts, 20 * MIB).await;
    let created = create(&api, &seeded, &["big.bin", "docs"], "tar_zst").await;
    let id = created["id"].as_str().unwrap();
    let before = list_files(&ts.config.data_dir);

    let response = api.get(created["url"].as_str().unwrap()).await;
    assert_eq!(response.status(), 200);
    let body = response.bytes().await.unwrap();
    assert!(body.len() > 20 * MIB);
    wait_state(&ts, id, "succeeded").await;

    assert!(list_files(&scratch).is_empty(), "TMPDIR is not empty");
    assert_eq!(list_files(&ts.config.data_dir), before);
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_zst_and_zip_formats() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let seeded = seed(&ts, 0).await;

        let created = create(&api, &seeded, &["docs", "b.txt"], "tar_zst").await;
        let response = api.get(created["url"].as_str().unwrap()).await;
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-type"], "application/zstd");
        assert!(created["file_name"].as_str().unwrap().ends_with(".tar.zst"));
        let tar_bytes = zstd::decode_all(&response.bytes().await.unwrap()[..]).unwrap();
        let files = tar_files(&tar_bytes);
        assert_eq!(files.len(), 3);
        assert_eq!(find(&files, "b.txt").unwrap(), b"bravo");

        let created = create(&api, &seeded, &["docs", "b.txt"], "zip").await;
        let response = api.get(created["url"].as_str().unwrap()).await;
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-type"], "application/zip");
        assert!(created["file_name"].as_str().unwrap().ends_with(".zip"));
        let bytes = response.bytes().await.unwrap();
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes.to_vec())).unwrap();
        let mut names = Vec::new();
        for i in 0..zip.len() {
            let mut file = zip.by_index(i).unwrap();
            if file.is_file() {
                let mut data = Vec::new();
                file.read_to_end(&mut data).unwrap();
                names.push((file.name().unwrap().into_owned(), data));
            }
        }
        assert_eq!(names.len(), 3, "[{}]", backend.name());
        let sub = names
            .iter()
            .find(|(n, _)| n.ends_with("docs/sub/c.txt"))
            .unwrap();
        assert_eq!(sub.1, b"charlie");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_running_marked_interrupted_on_restart() {
    for backend in backends() {
        let mut ts = TestServer::start(&backend).await;
        let seeded = seed(&ts, 0).await;
        sqlx::query(
            "INSERT INTO downloads (id, repository_id, hostname, snapshot_id, items_json, format, \
             started_by, state, created_at, started_at) \
             VALUES ('d-running', $1, 'host-a', $2, '[]', 'tar_gz', 'admin', 'running', 1, 2)",
        )
        .bind(&seeded.repo)
        .bind(&seeded.snapshot)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
        ts.restart().await;
        let (state, _, code, _) = row(&ts, "d-running").await;
        assert_eq!(state, "interrupted", "[{}]", backend.name());
        assert_eq!(code.as_deref(), Some("server_restarted"));
        ts.stop().await;
    }
}

async fn insert_download(
    ts: &TestServer,
    id: &str,
    repository: &str,
    snapshot: &str,
    state: &str,
    (created_at, started_at): (i64, Option<i64>),
) {
    sqlx::query(
        "INSERT INTO downloads (id, repository_id, hostname, snapshot_id, items_json, format, \
         started_by, state, created_at, started_at) \
         VALUES ($1, $2, 'host-a', $3, $4, 'tar_gz', 'admin', $5, $6, $7)",
    )
    .bind(id)
    .bind(repository)
    .bind(snapshot)
    .bind(json!([b64(b"/file")]).to_string())
    .bind(state)
    .bind(created_at)
    .bind(started_at)
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_cancelled_before_streaming_leaves_no_running_row() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", silent.local_addr().unwrap());
        let held = tokio::spawn(async move {
            let mut connections = Vec::new();
            while let Ok((connection, _)) = silent.accept().await {
                connections.push(connection);
            }
        });
        let repo = create_repository(
            ts.state(),
            NewRepository {
                name: "silent".to_string(),
                location: RepoLocation {
                    backend: "s3".to_string(),
                    options: BTreeMap::from([
                        ("bucket".to_string(), "silent".to_string()),
                        ("endpoint".to_string(), endpoint),
                    ]),
                },
                experimental: false,
                format_repo_id: [1; 16],
                config_hash: [2; 32],
                master: MasterKey::from_bytes([3; 32]),
                server_slot_secret: None,
                server_creds: Some(AccessKey {
                    access_key_id: "server".to_string(),
                    secret_access_key: "server-secret".to_string(),
                }),
                agent_creds: None,
            },
        )
        .await
        .unwrap();
        let id = "d-cancelled";
        let snapshot = "0".repeat(64);
        insert_download(&ts, id, &repo, &snapshot, "pending", (now_ms(), None)).await;

        let url = format!("/api/v1/downloads/{id}/archive");
        let mut request = Box::pin(api.get(&url));
        tokio::select! {
            response = &mut request => panic!("silent storage answered: {}", response.status()),
            () = wait_state(&ts, id, "running") => {}
        }
        drop(request);

        wait_state(&ts, id, "interrupted").await;
        assert_eq!(
            row(&ts, id).await.2.as_deref(),
            Some("client_closed"),
            "[{}]",
            backend.name()
        );
        held.abort();
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn janitor_frees_running_downloads_past_their_deadline() {
    let hour = 3_600_000;
    for (demo, kept, reaped) in [
        (false, now_ms() - hour, now_ms() - 25 * hour),
        (true, now_ms() - 1000, now_ms() - hour),
    ] {
        for backend in backends() {
            let ts = TestServer::start_with(&backend, |c| {
                c.schedule_tick_secs = 0;
                c.demo_mode = demo;
                c.demo.download_timeout = Duration::from_secs(120);
            })
            .await;
            let seeded = seed(&ts, 0).await;
            for (id, started) in [("d-kept", kept), ("d-reaped", reaped)] {
                insert_download(
                    &ts,
                    id,
                    &seeded.repo,
                    &seeded.snapshot,
                    "running",
                    (started, Some(started)),
                )
                .await;
            }

            let stop = CancellationToken::new();
            let janitor = tokio::spawn(session_janitor(ts.state().clone(), stop.clone()));
            wait_state(&ts, "d-reaped", "interrupted").await;
            assert_eq!(row(&ts, "d-reaped").await.2.as_deref(), Some("timed_out"));
            assert_eq!(row(&ts, "d-kept").await.0, "running", "demo {demo}");
            stop.cancel();
            janitor.await.unwrap().unwrap();
            ts.stop().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn janitor_deletes_links_left_unused() {
    let hour = 3_600_000;
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let seeded = seed(&ts, 0).await;
        for (id, created) in [("d-recent", now_ms()), ("d-old", now_ms() - 2 * hour)] {
            insert_download(
                &ts,
                id,
                &seeded.repo,
                &seeded.snapshot,
                "pending",
                (created, None),
            )
            .await;
        }

        let stop = CancellationToken::new();
        let janitor = tokio::spawn(session_janitor(ts.state().clone(), stop.clone()));
        let pool = ts.state().db.pool.clone();
        let deleted = wait_until(Duration::from_secs(10), || {
            let pool = pool.clone();
            async move {
                sqlx::query("SELECT 1 AS one FROM downloads WHERE id = 'd-old'")
                    .fetch_optional(&pool)
                    .await
                    .unwrap()
                    .is_none()
            }
        })
        .await;
        assert!(deleted, "[{}] the old link is still stored", backend.name());
        assert_eq!(row(&ts, "d-recent").await.0, "pending");
        stop.cancel();
        janitor.await.unwrap().unwrap();
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_stores_each_selected_path_once() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let seeded = seed(&ts, 0).await;
        let created = create(&api, &seeded, &["b.txt", "docs", "b.txt"], "tar_gz").await;
        let stored: String = sqlx::query("SELECT items_json FROM downloads WHERE id = $1")
            .bind(created["id"].as_str().unwrap())
            .fetch_one(&ts.state().db.pool)
            .await
            .unwrap()
            .try_get("items_json")
            .unwrap();
        let items: Vec<String> = serde_json::from_str(&stored).unwrap();
        assert_eq!(items.len(), 2, "[{}]", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_validation() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let seeded = seed(&ts, 0).await;
        let item = seeded.item("b.txt");
        let oversized: Vec<String> = (0..4000)
            .map(|i| b64(format!("/{i:0>60}").as_bytes()))
            .collect();
        let cases = [
            (
                "unknown snapshot",
                json!({"snapshot_id": "0".repeat(64), "items": [item], "format": "zip"}),
                404,
            ),
            (
                "not a snapshot id",
                json!({"snapshot_id": "nope", "items": [item], "format": "zip"}),
                404,
            ),
            (
                "no items",
                json!({"snapshot_id": seeded.snapshot, "items": [], "format": "zip"}),
                422,
            ),
            (
                "bad base64",
                json!({"snapshot_id": seeded.snapshot, "items": ["***"], "format": "zip"}),
                422,
            ),
            (
                "relative item",
                json!({"snapshot_id": seeded.snapshot, "items": [b64(b"rel")], "format": "zip"}),
                422,
            ),
            (
                "missing item",
                json!({"snapshot_id": seeded.snapshot, "items": [seeded.item("nope.txt")], "format": "zip"}),
                404,
            ),
            (
                "unknown format",
                json!({"snapshot_id": seeded.snapshot, "items": [item], "format": "rar"}),
                422,
            ),
            (
                "selection too large to store",
                json!({"snapshot_id": seeded.snapshot, "items": oversized, "format": "zip"}),
                422,
            ),
        ];
        for (name, payload, want) in cases {
            let status = api.post("/api/v1/downloads", &payload).await.status();
            assert_eq!(status, want, "{name} [{}]", backend.name());
        }
        let rows: i64 = sqlx::query("SELECT COUNT(*) AS n FROM downloads")
            .fetch_one(&ts.state().db.pool)
            .await
            .unwrap()
            .try_get("n")
            .unwrap();
        assert_eq!(rows, 0);
        ts.stop().await;
    }
}
