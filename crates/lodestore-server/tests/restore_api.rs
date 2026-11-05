mod common;

use common::{Api, TestServer, backends};
use data_encoding::BASE64URL_NOPAD;
use lodestore_agent::{EnrollArgs, NetOptions};
use lodestore_format::index::hex32;
use lodestore_format::repo::Repository;
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_server::catalog::ingest_snapshot;
use serde_json::{Value, json};

const PASSWORD: &[u8] = b"test-password";

type Tweak<'a> = &'a dyn Fn(&mut Value);

fn b64(bytes: &[u8]) -> String {
    BASE64URL_NOPAD.encode(bytes)
}

async fn enroll(ts: &TestServer, api: &Api, repo: &str, hostname: &str) -> String {
    let token = api.join_token(repo, json!([])).await;
    lodestore_agent::enroll(EnrollArgs {
        token,
        paths: ts.agent_paths(hostname),
        hostname: Some(hostname.to_string()),
        net: NetOptions::default(),
    })
    .await
    .expect("enroll");
    api.hosts()
        .await
        .iter()
        .find(|h| h["hostname"] == hostname)
        .map(|h| h["id"].as_str().unwrap().to_string())
        .expect("the enrolled host")
}

struct Seeded {
    snapshot: String,
    base: String,
}

impl Seeded {
    fn item(&self, below: &str) -> String {
        b64(format!("{}/{below}", self.base).as_bytes())
    }
}

async fn seed(ts: &TestServer, repo_id: &str) -> Seeded {
    let store = LocalFsStore::new(ts.local_repo_root("main"));
    let opened = Repository::open(&store, PASSWORD).await.unwrap();
    let keys = opened.read_keys().write_keys(opened.config_hash());
    let mut writer = Writer::new(&store, keys, opened.config()).await.unwrap();
    let data = ts.dir.path().join("tree");
    std::fs::create_dir_all(data.join("docs")).unwrap();
    std::fs::write(data.join("docs/a.txt"), b"alpha").unwrap();
    std::fs::write(data.join("b.txt"), b"bravo").unwrap();
    let outcome = writer
        .backup(
            std::slice::from_ref(&data),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .expect("backup");
    let snapshot = hex32(&outcome.snapshot_id);
    ingest_snapshot(ts.state(), repo_id, &snapshot, None)
        .await
        .expect("ingest");
    Seeded {
        snapshot,
        base: data.canonicalize().unwrap().display().to_string(),
    }
}

fn body(seeded: &Seeded, host: &str) -> Value {
    json!({
        "snapshot_id": seeded.snapshot,
        "items": [seeded.item("docs"), seeded.item("b.txt")],
        "target_host_id": host,
        "destination": "/srv/out",
        "in_place": false,
        "conflict": "skip_existing",
        "restore_owners": true,
        "accept_unverified": true,
    })
}

async fn get_json(api: &Api, path: &str) -> Value {
    let response = api.get(path).await;
    assert_eq!(response.status(), 200, "{path}");
    response.json().await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_api_creates_queued_job() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let seeded = seed(&ts, &repo).await;

        let mut payload = body(&seeded, &host);
        payload["destination"] = json!("/srv//./out/./");
        let response = api.post("/api/v1/restores", &payload).await;
        assert_eq!(response.status(), 201, "[{}]", backend.name());
        let created: Value = response.json().await.unwrap();
        let job_id = created["job_id"].as_str().unwrap().to_string();

        let list = get_json(&api, "/api/v1/jobs?kind=restore").await;
        assert_eq!(list["total"], 1);
        let job = &list["items"][0];
        assert_eq!(job["id"], job_id);
        assert_eq!(job["kind"], "restore");
        assert_eq!(job["state"], "queued");
        assert_eq!(job["hostname"], "host-a");
        assert_eq!(job["restore"]["destination"], "/srv/out");
        assert_eq!(job["restore"]["conflict"], "skip_existing");
        assert_eq!(job["restore"]["restore_owners"], true);
        assert_eq!(job["restore"]["items"], 2);
        assert_eq!(job["restore"]["started_by"], "admin");
        assert_eq!(job["restore"]["other_repository"], false);
        let source = job["restore"]["source_hostname"].as_str().unwrap();
        assert_eq!(
            job["restore"]["source_host_key"],
            format!("agentless:{source}")
        );
        assert!(job["restore_result"].is_null());
        assert!(job["stats"].is_null());

        let detail = get_json(&api, &format!("/api/v1/jobs/{job_id}")).await;
        assert_eq!(detail["kind"], "restore");
        assert_eq!(detail["restore"]["snapshot_id"], seeded.snapshot);
        assert_eq!(detail["restore"]["started_by"], "admin");

        let backups = get_json(&api, "/api/v1/jobs?kind=backup").await;
        assert_eq!(backups["total"], 0);

        sqlx::query(
            "UPDATE jobs SET state = 'succeeded', finished_at = 5, phase = 'restore_files', \
             stats_json = $1 WHERE id = $2",
        )
        .bind(
            json!({"created": 3, "overwritten": 1, "skipped": 2, "files": 4, "bytes": 99})
                .to_string(),
        )
        .bind(&job_id)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
        let done = get_json(&api, &format!("/api/v1/jobs/{job_id}")).await;
        assert_eq!(done["phase"], "restore_files");
        assert_eq!(done["restore_result"]["created"], 3);
        assert_eq!(done["restore_result"]["skipped"], 2);
        assert_eq!(done["restore_result"]["bytes"], 99);
        assert!(done["stats"].is_null());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_api_validates() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let seeded = seed(&ts, &repo).await;

        let status = |tweak: &dyn Fn(&mut Value)| {
            let mut payload = body(&seeded, &host);
            tweak(&mut payload);
            let api = &api;
            async move {
                api.post("/api/v1/restores", &payload)
                    .await
                    .status()
                    .as_u16()
            }
        };
        let cases: [(&str, Tweak, u16); 17] = [
            (
                "dotdot destination",
                &|b| b["destination"] = json!("/tmp/../etc"),
                422,
            ),
            (
                "dotdot to root",
                &|b| b["destination"] = json!("/tmp/.."),
                422,
            ),
            (
                "missing item",
                &|b| b["items"] = json!([seeded.item("nope.txt")]),
                404,
            ),
            ("root", &|b| b["destination"] = json!("/"), 400),
            ("double slash", &|b| b["destination"] = json!("//"), 400),
            ("root dot", &|b| b["destination"] = json!("/."), 400),
            ("root dot slash", &|b| b["destination"] = json!("/./"), 400),
            (
                "in place, stop if not empty",
                &|b| {
                    b["destination"] = json!("/");
                    b["in_place"] = json!(true);
                    b["conflict"] = json!("stop_if_not_empty");
                },
                400,
            ),
            (
                "relative item",
                &|b| b["items"] = json!([b64(b"relative")]),
                422,
            ),
            ("empty selection", &|b| b["items"] = json!([]), 422),
            ("bad base64", &|b| b["items"] = json!(["***"]), 422),
            (
                "dotdot item",
                &|b| b["items"] = json!([b64(b"/a/../b")]),
                422,
            ),
            (
                "relative destination",
                &|b| b["destination"] = json!("out"),
                422,
            ),
            ("in place elsewhere", &|b| b["in_place"] = json!(true), 422),
            (
                "unknown snapshot",
                &|b| b["snapshot_id"] = json!("0".repeat(64)),
                404,
            ),
            (
                "unknown host",
                &|b| b["target_host_id"] = json!("nobody"),
                404,
            ),
            (
                "unknown conflict",
                &|b| b["conflict"] = json!("replace"),
                422,
            ),
        ];
        for (name, tweak, want) in cases {
            assert_eq!(status(tweak).await, want, "{name} [{}]", backend.name());
        }
        for (destination, in_place, conflict, code) in [
            ("/./", false, "skip_existing", "restore_destination_root"),
            ("/", true, "stop_if_not_empty", "restore_in_place_not_empty"),
        ] {
            let mut payload = body(&seeded, &host);
            payload["destination"] = json!(destination);
            payload["in_place"] = json!(in_place);
            payload["conflict"] = json!(conflict);
            let error: Value = api
                .post("/api/v1/restores", &payload)
                .await
                .json()
                .await
                .unwrap();
            assert_eq!(error["code"], code);
        }

        sqlx::query("UPDATE agents SET status = 'revoked' WHERE id = $1")
            .bind(&host)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        assert_eq!(status(&|_| {}).await, 422, "revoked host");
        let list = get_json(&api, "/api/v1/jobs?kind=restore").await;
        assert_eq!(list["total"], 0);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_api_other_repository_flag() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let main = ts.create_local_repository("main").await;
        let second = ts.create_local_repository("second").await;
        let same = enroll(&ts, &api, &main, "host-a").await;
        let other = enroll(&ts, &api, &second, "host-b").await;
        let seeded = seed(&ts, &main).await;

        for (host, want) in [(&same, false), (&other, true)] {
            let response = api.post("/api/v1/restores", &body(&seeded, host)).await;
            assert_eq!(response.status(), 201);
            let created: Value = response.json().await.unwrap();
            let detail = get_json(
                &api,
                &format!("/api/v1/jobs/{}", created["job_id"].as_str().unwrap()),
            )
            .await;
            assert_eq!(
                detail["restore"]["other_repository"],
                want,
                "[{}]",
                backend.name()
            );
        }
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_api_rerun_refused() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let seeded = seed(&ts, &repo).await;

        let created: Value = api
            .post("/api/v1/restores", &body(&seeded, &host))
            .await
            .json()
            .await
            .unwrap();
        let id = created["job_id"].as_str().unwrap();

        let rerun = api
            .post(&format!("/api/v1/jobs/{id}/rerun"), &json!({}))
            .await;
        assert_eq!(rerun.status(), 409, "[{}]", backend.name());

        let cancel = api
            .post(&format!("/api/v1/jobs/{id}/cancel"), &json!({}))
            .await;
        assert_eq!(cancel.status(), 200);
        let cancelled: Value = cancel.json().await.unwrap();
        assert_eq!(cancelled["state"], "cancelled");
        assert_eq!(cancelled["kind"], "restore");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_and_download_name_the_host_like_the_host_list() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let seeded = seed(&ts, &repo).await;
        sqlx::query("UPDATE snapshots SET host_id = $1, hostname = 'Box.local' WHERE id = $2")
            .bind(&host)
            .bind(&seeded.snapshot)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();

        let response = api.post("/api/v1/restores", &body(&seeded, &host)).await;
        assert_eq!(response.status(), 201, "[{}]", backend.name());
        let list = get_json(&api, "/api/v1/jobs?kind=restore").await;
        assert_eq!(list["items"][0]["restore"]["source_hostname"], "host-a");
        assert_eq!(list["items"][0]["restore"]["source_host_key"], host);

        let response = api
            .post(
                "/api/v1/downloads",
                &json!({
                    "snapshot_id": seeded.snapshot,
                    "items": [seeded.item("b.txt")],
                    "format": "zip",
                }),
            )
            .await;
        assert_eq!(response.status(), 201);
        let created: Value = response.json().await.unwrap();
        assert_eq!(
            created["file_name"],
            format!("host-a-{}.zip", &seeded.snapshot[..8])
        );
        let downloads = get_json(&api, "/api/v1/jobs?kind=download").await;
        assert_eq!(downloads["items"][0]["hostname"], "host-a");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_api_unverified_needs_confirmation() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let seeded = seed(&ts, &repo).await;

        let mut payload = body(&seeded, &host);
        payload["accept_unverified"] = json!(false);
        let response = api.post("/api/v1/restores", &payload).await;
        assert_eq!(response.status(), 400, "[{}]", backend.name());
        let error: Value = response.json().await.unwrap();
        assert_eq!(error["code"], "restore_unverified_not_confirmed");
        payload.as_object_mut().unwrap().remove("accept_unverified");
        let response = api.post("/api/v1/restores", &payload).await;
        assert_eq!(response.status(), 400);

        payload["accept_unverified"] = json!(true);
        let response = api.post("/api/v1/restores", &payload).await;
        assert_eq!(response.status(), 201);

        sqlx::query(
            "UPDATE snapshots SET verified_at = received_at, verified_by = 'server' WHERE id = $1",
        )
        .bind(&seeded.snapshot)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
        payload["accept_unverified"] = json!(false);
        let response = api.post("/api/v1/restores", &payload).await;
        assert_eq!(response.status(), 201);
        ts.stop().await;
    }
}
