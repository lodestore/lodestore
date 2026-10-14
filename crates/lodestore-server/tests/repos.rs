mod common;

use std::path::PathBuf;

use common::{Api, Backend, TestServer, backends};
use lodestore_agent::{EnrollArgs, NetOptions};
use lodestore_format::keys::WriteKeys;
use lodestore_format::repo::Repository;
use lodestore_format::store::{BlobStore, LocalFsStore};
use lodestore_server::repos::db::server_access;
use serde_json::{Value, json};

const PASSWORD: &str = "correct horse battery";

async fn body(response: reqwest::Response) -> Value {
    response.json().await.expect("a JSON body")
}

fn local_draft(root: &std::path::Path, name: &str) -> Value {
    json!({
        "name": name,
        "location": { "backend": "local", "options": { "path": root.display().to_string() } },
    })
}

fn last_group(recovery_key: &str) -> String {
    recovery_key.rsplit('-').next().unwrap().to_string()
}

fn other_group(group: &str) -> &'static str {
    if group.eq_ignore_ascii_case("AAAA") {
        "BBBB"
    } else {
        "AAAA"
    }
}

struct Created {
    id: String,
    root: PathBuf,
    recovery_key: String,
}

async fn create_local(ts: &TestServer, api: &Api, name: &str) -> Created {
    let root = ts.local_repo_root(name);
    let draft = local_draft(&root, name);
    let started = api.post("/api/v1/repositories/wizard", &draft).await;
    assert_eq!(started.status(), 201);
    let started = body(started).await;
    let recovery_key = started["recovery_key"].as_str().unwrap().to_string();
    let committed = api
        .post(
            &format!(
                "/api/v1/repositories/wizard/{}/commit",
                started["wizard_id"].as_str().unwrap()
            ),
            &json!({ "password": PASSWORD, "recovery_key_last_group": last_group(&recovery_key) }),
        )
        .await;
    assert_eq!(committed.status(), 201);
    let committed = body(committed).await;
    Created {
        id: committed["id"].as_str().unwrap().to_string(),
        root,
        recovery_key,
    }
}

async fn enroll(ts: &TestServer, api: &Api, repository: &Created, name: &str) -> (Value, String) {
    let token = api.join_token(&repository.id, json!([])).await;
    let paths = ts.agent_paths(name);
    lodestore_agent::enroll(EnrollArgs {
        token,
        paths: paths.clone(),
        hostname: Some(format!("host-{name}")),
        net: NetOptions::default(),
    })
    .await
    .expect("enroll");
    let keys_text = std::fs::read_to_string(paths.writer_keys()).unwrap();
    let repository_json: Value =
        serde_json::from_slice(&std::fs::read(paths.repository_json()).unwrap()).unwrap();
    (repository_json, keys_text)
}

async fn assert_keys_match(root: &std::path::Path, keys_text: &str) {
    let keys = WriteKeys::from_file_text(keys_text).expect("a valid writer keys file");
    let config = LocalFsStore::new(root.to_path_buf())
        .get("config")
        .await
        .expect("repository config");
    keys.verify_config(&config)
        .expect("the keys match this repository's config");
}

async fn local_wizard_creates_repository(backend: &Backend) {
    let ts = TestServer::start(backend).await;
    let api = ts.api().await;
    let root = ts.local_repo_root("main");
    let draft = local_draft(&root, "main");

    let checked = api.post("/api/v1/repositories/check", &draft).await;
    assert_eq!(checked.status(), 200);
    let checked = body(checked).await;
    let rows: Vec<(String, String)> = checked["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["id"].as_str().unwrap().to_string(),
                c["status"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let expected = [
        ("write", "pass"),
        ("list", "pass"),
        ("conditional_write", "pass"),
        ("agent_delete", "neutral"),
        ("object_lock", "neutral"),
    ];
    assert_eq!(
        rows,
        expected.map(|(a, b)| (a.to_string(), b.to_string())),
        "[{}]",
        backend.name()
    );
    assert_eq!(checked["blocking"], false);
    assert_eq!(checked["existing_repository"], false);

    let started = api.post("/api/v1/repositories/wizard", &draft).await;
    assert_eq!(started.status(), 201);
    assert_eq!(started.headers()["cache-control"], "no-store");
    let started = body(started).await;
    let recovery_key = started["recovery_key"].as_str().unwrap().to_string();
    assert!(recovery_key.starts_with("lode-rk1-"), "{recovery_key}");
    let commit_path = format!(
        "/api/v1/repositories/wizard/{}/commit",
        started["wizard_id"].as_str().unwrap()
    );
    assert!(
        !root.join("config").exists(),
        "nothing is written before the key is confirmed"
    );

    let group = last_group(&recovery_key);
    let wrong = api
        .post(
            &commit_path,
            &json!({ "password": PASSWORD, "recovery_key_last_group": other_group(&group) }),
        )
        .await;
    assert_eq!(wrong.status(), 422);
    assert!(
        !root.join("config").exists(),
        "a wrong group writes nothing"
    );

    let short = api
        .post(
            &commit_path,
            &json!({ "password": "elevenchars", "recovery_key_last_group": group }),
        )
        .await;
    assert_eq!(short.status(), 422);
    assert!(!root.join("config").exists());

    let spaced = format!("  {}  ", group.to_ascii_lowercase());
    let created = api
        .post(
            &commit_path,
            &json!({ "password": PASSWORD, "recovery_key_last_group": spaced }),
        )
        .await;
    assert_eq!(created.status(), 201);
    let created = body(created).await;
    assert_eq!(created["name"], "main");
    assert_eq!(created["agent_key_state"], "unchecked");

    assert!(root.join("config").is_file());
    assert_eq!(std::fs::read_dir(root.join("keys")).unwrap().count(), 3);
    let repository = Repository::open(&LocalFsStore::new(root.clone()), PASSWORD.as_bytes()).await;
    assert!(
        repository.is_ok(),
        "the admin password opens the repository"
    );

    let list = body(api.get("/api/v1/repositories").await).await;
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], created["id"]);
    assert_eq!(list[0]["agent_key_state"], "unchecked");
    assert_eq!(list[0]["backend"], "local");

    let again = api
        .post(
            &commit_path,
            &json!({ "password": PASSWORD, "recovery_key_last_group": group }),
        )
        .await;
    assert_eq!(again.status(), 404, "a wizard session is single use");

    let repo = Created {
        id: created["id"].as_str().unwrap().to_string(),
        root: root.clone(),
        recovery_key,
    };
    let (repository_json, keys_text) = enroll(&ts, &api, &repo, "one").await;
    assert_eq!(repository_json["repository_id"], created["id"]);
    assert_keys_match(&root, &keys_text).await;
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wizard_local_creates_repository() {
    for backend in backends() {
        local_wizard_creates_repository(&backend).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repository_get_does_not_touch_storage() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = create_local(&ts, &api, "main").await;
        let path = format!("/api/v1/repositories/{}", repo.id);

        std::fs::remove_dir_all(&repo.root).unwrap();
        let detail = body(api.get(&path).await).await;
        assert_eq!(detail["unlocked"], true, "[{}]", backend.name());

        let rechecked = api.post(&format!("{path}/check"), &json!({})).await;
        assert_eq!(rechecked.status(), 200);
        let detail = body(api.get(&path).await).await;
        assert_eq!(detail["unlocked"], false, "[{}]", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wizard_restart_unlocks() {
    for backend in backends() {
        let mut ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = create_local(&ts, &api, "main").await;

        ts.restart().await;
        let api = ts.api().await;
        let path = format!("/api/v1/repositories/{}", repo.id);
        let detail = body(api.get(&path).await).await;
        assert_eq!(detail["unlocked"], Value::Null, "[{}]", backend.name());

        let rechecked = api.post(&format!("{path}/check"), &json!({})).await;
        assert_eq!(rechecked.status(), 200);
        let detail = body(api.get(&path).await).await;
        assert_eq!(detail["unlocked"], true, "[{}]", backend.name());

        let list = body(api.get("/api/v1/repositories").await).await;
        assert_eq!(list[0]["unlocked"], true);

        let (_, keys_text) = enroll(&ts, &api, &repo, "after-restart").await;
        assert_keys_match(&repo.root, &keys_text).await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repos_multiple_repositories() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let first = create_local(&ts, &api, "first").await;
        let second = create_local(&ts, &api, "second").await;
        assert_ne!(first.id, second.id);

        let (json_a, keys_a) = enroll(&ts, &api, &first, "a").await;
        let (json_b, keys_b) = enroll(&ts, &api, &second, "b").await;
        assert_eq!(json_a["repository_id"], first.id);
        assert_eq!(json_b["repository_id"], second.id);
        assert_keys_match(&first.root, &keys_a).await;
        assert_keys_match(&second.root, &keys_b).await;
        assert!(
            WriteKeys::from_file_text(&keys_a)
                .unwrap()
                .verify_config(&std::fs::read(second.root.join("config")).unwrap())
                .is_err(),
            "the first repository's keys must not fit the second"
        );

        let list = body(api.get("/api/v1/repositories").await).await;
        let list = list.as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|r| r["hosts_count"] == 1));

        let duplicate = api
            .post(
                "/api/v1/repositories/wizard",
                &local_draft(&ts.local_repo_root("third"), "first"),
            )
            .await;
        assert_eq!(duplicate.status(), 409);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repos_no_secrets_in_responses() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = create_local(&ts, &api, "main").await;

        let access = server_access(ts.state(), &repo.id).await.unwrap();
        let slot = access.server_slot_secret.expect("a server slot secret");
        let needles = [
            PASSWORD.to_string(),
            repo.recovery_key.clone(),
            repo.recovery_key.to_ascii_lowercase(),
            "recovery_key".to_string(),
            data_encoding::HEXLOWER.encode(&slot),
            data_encoding::HEXLOWER.encode(&access.master),
            data_encoding::BASE64.encode(&slot),
            data_encoding::BASE64.encode(&access.master),
        ];
        for path in [
            "/api/v1/repositories".to_string(),
            format!("/api/v1/repositories/{}", repo.id),
        ] {
            let text = api.get(&path).await.text().await.unwrap();
            for needle in &needles {
                assert!(!text.contains(needle.as_str()), "{path} leaks a secret");
            }
        }

        let upper = api
            .post(
                "/api/v1/repositories/wizard",
                &local_draft(&ts.local_repo_root("upper"), "Main-Repo"),
            )
            .await;
        assert_eq!(upper.status(), 422);
        ts.stop().await;
    }
}
