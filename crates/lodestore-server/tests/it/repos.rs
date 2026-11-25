use crate::common;

use std::path::PathBuf;

use common::{Api, Backend, TestServer, backends};
use lodestore_agent::{EnrollArgs, NetOptions};
use lodestore_format::keys::WriteKeys;
use lodestore_format::repo::Repository;
use lodestore_format::store::{BlobStore, LocalFsStore};
use lodestore_server::repos::db::server_access;
use serde_json::{Value, json};
use sqlx::Row;

const PASSWORD: &str = "correct horse battery";

async fn body(response: reqwest::Response) -> Value {
    response.json().await.expect("a JSON body")
}

fn local_draft(root: &std::path::Path, name: &str) -> Value {
    json!({
        "name": name,
        "location": { "backend": "local", "options": { "path": root.display().to_string() } },
        "accept_weaker_guarantee": true,
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
async fn wizard_local_folder_needs_consent() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let mut draft = local_draft(&ts.local_repo_root("main"), "main");
        draft["accept_weaker_guarantee"] = json!(false);

        let refused = api.post("/api/v1/repositories/wizard", &draft).await;
        assert_eq!(refused.status(), 409, "[{}]", backend.name());
        let refused = body(refused).await;
        assert_eq!(refused["code"], "weaker_guarantee");
        assert_eq!(
            refused["check"]["guarantee_level"],
            "host_can_overwrite_or_delete"
        );
        assert!(
            refused["message"]
                .as_str()
                .unwrap()
                .contains("Every host that writes to this folder"),
            "{refused}"
        );
        let list = body(api.get("/api/v1/repositories").await).await;
        assert_eq!(list.as_array().unwrap().len(), 0);

        draft["accept_weaker_guarantee"] = json!(true);
        let started = api.post("/api/v1/repositories/wizard", &draft).await;
        assert_eq!(started.status(), 201);
        let started = body(started).await;
        let key = started["recovery_key"].as_str().unwrap();
        let committed = api
            .post(
                &format!(
                    "/api/v1/repositories/wizard/{}/commit",
                    started["wizard_id"].as_str().unwrap()
                ),
                &json!({ "password": PASSWORD, "recovery_key_last_group": last_group(key) }),
            )
            .await;
        assert_eq!(committed.status(), 201);

        let list = body(api.get("/api/v1/repositories").await).await;
        assert_eq!(list[0]["guarantee_level"], "host_can_overwrite_or_delete");
        assert_eq!(
            list[0]["guarantee_accepted"],
            "host_can_overwrite_or_delete"
        );
        assert!(list[0]["guarantee_checked_at"].is_i64());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connect_existing_needs_consent() {
    for backend in backends() {
        let first = TestServer::start(&backend).await;
        first.create_local_repository("existing").await;
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let mut request = json!({
            "draft": {
                "name": "existing",
                "location": {
                    "backend": "local",
                    "options": { "path": first.local_repo_root("existing").display().to_string() },
                },
            },
            "password": "test-password",
            "recovery_key": null,
        });

        let refused = api.post("/api/v1/repositories/connect", &request).await;
        assert_eq!(refused.status(), 409, "[{}]", backend.name());
        let refused = body(refused).await;
        assert_eq!(refused["code"], "weaker_guarantee");
        assert!(
            refused["message"]
                .as_str()
                .unwrap()
                .contains("connect the repository anyway"),
            "{refused}"
        );
        let list = body(api.get("/api/v1/repositories").await).await;
        assert_eq!(list.as_array().unwrap().len(), 0);

        request["draft"]["accept_weaker_guarantee"] = json!(true);
        let connected = api.post("/api/v1/repositories/connect", &request).await;
        assert_eq!(connected.status(), 201, "[{}]", backend.name());
        let connected = body(connected).await;
        assert_eq!(
            connected["guarantee_accepted"],
            "host_can_overwrite_or_delete"
        );
        ts.stop().await;
        first.stop().await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repos_refuse_unknown_location_options() {
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;
    let keys = json!({ "access_key_id": "AKID", "secret_access_key": "KEYSECRET" });
    let cases = [
        (
            "azblob",
            json!({ "container": "c", "account_name": "acct", "account_key": "a2V5", "sas_key": "SECRET" }),
            "sas_key",
        ),
        ("gcs", json!({ "bucket": "b", "Token": "SECRET" }), "Token"),
        (
            "s3",
            json!({ "bucket": "b", "secret_access_key": "SECRET" }),
            "secret_access_key",
        ),
        (
            "sftp",
            json!({ "endpoint": "h", "user": "u", "key": "k", "host_key": "h", "password": "SECRET" }),
            "password",
        ),
        (
            "local",
            json!({ "path": "/srv/repo", "Path": "/x" }),
            "Path",
        ),
    ];
    for (backend, options, unknown) in cases {
        let draft = json!({
            "name": "unknown",
            "location": { "backend": backend, "options": options },
            "server_credentials": keys,
            "agent_credentials": keys,
            "accept_weaker_guarantee": true,
        });
        let connect = json!({ "draft": draft, "password": PASSWORD, "recovery_key": null });
        for (path, request) in [
            ("/api/v1/repositories/check", &draft),
            ("/api/v1/repositories/wizard", &draft),
            ("/api/v1/repositories/connect", &connect),
        ] {
            let response = api.post(path, request).await;
            assert_eq!(response.status(), 422, "{backend} {path}");
            let message = body(response).await["message"].as_str().unwrap().to_owned();
            assert!(
                message.contains(&format!("`{unknown}`")),
                "{backend} {path}: {message}"
            );
            assert!(!message.contains("SECRET"), "{message}");
        }
    }
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn migration_0037_carries_the_current_level_as_accepted() {
    let cases = [
        ("loc", "local", "unchecked", "host_can_overwrite_or_delete"),
        ("ver", "s3", "verified", "create_only_enforced"),
        ("ovr", "s3", "can_overwrite", "host_can_overwrite_or_delete"),
        ("unc", "s3", "unchecked", "unchecked"),
    ];
    for backend in backends() {
        let mut ts = TestServer::start(&backend).await;
        let pool = ts.state().db.pool.clone();
        let name = backend.name();
        for (repo, kind, state, _) in cases {
            ts.create_local_repository(repo).await;
            sqlx::query(
                "UPDATE repositories SET backend = $1, agent_key_state = $2 WHERE name = $3",
            )
            .bind(kind)
            .bind(state)
            .bind(repo)
            .execute(&pool)
            .await
            .unwrap();
        }
        for statement in [
            "ALTER TABLE repositories DROP COLUMN guarantee_level",
            "ALTER TABLE repositories DROP COLUMN guarantee_accepted",
            "ALTER TABLE repositories DROP COLUMN guarantee_notes_json",
            "ALTER TABLE repositories DROP COLUMN guarantee_checked_at",
            "DELETE FROM _sqlx_migrations WHERE version = 37",
        ] {
            sqlx::query(statement).execute(&pool).await.unwrap();
        }

        ts.restart().await;
        let pool = ts.state().db.pool.clone();

        for (repo, _, _, accepted) in cases {
            let row = sqlx::query(
                "SELECT guarantee_level, guarantee_accepted, guarantee_notes_json, \
                 guarantee_checked_at FROM repositories WHERE name = $1",
            )
            .bind(repo)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(
                row.try_get::<String, _>("guarantee_accepted").unwrap(),
                accepted,
                "[{name}] {repo}"
            );
            assert_eq!(
                row.try_get::<String, _>("guarantee_level").unwrap(),
                "unchecked",
                "[{name}] {repo}"
            );
            assert_eq!(
                row.try_get::<String, _>("guarantee_notes_json").unwrap(),
                "[]",
                "[{name}] {repo}"
            );
            assert_eq!(
                row.try_get::<Option<i64>, _>("guarantee_checked_at")
                    .unwrap(),
                None,
                "[{name}] {repo}"
            );
        }
        let verified = sqlx::query(
            "SELECT guarantee_level, guarantee_accepted FROM repositories WHERE name = 'ver'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            verified.try_get::<String, _>("guarantee_level").unwrap(),
            "unchecked"
        );
        assert_eq!(
            verified.try_get::<String, _>("guarantee_accepted").unwrap(),
            "create_only_enforced"
        );
        ts.stop().await;
    }
}

async fn accepted(ts: &TestServer, id: &str) -> String {
    sqlx::query("SELECT guarantee_accepted FROM repositories WHERE id = $1")
        .bind(id)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .try_get("guarantee_accepted")
        .unwrap()
}

async fn accept_now(ts: &TestServer, id: &str, level: &str) {
    sqlx::query("UPDATE repositories SET guarantee_accepted = $1 WHERE id = $2")
        .bind(level)
        .bind(id)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
}

/// A check reads the accepted level, someone accepts another one, and then
/// the check stores what it decided from the old value: the accept stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guarantee_write_never_overwrites_an_accept_in_between() {
    use lodestore_server::repos::db::{
        AccessKey, CheckWrite, GuaranteeWrite, Stored, replace_agent_creds, update_checks,
    };
    fn write(g: &GuaranteeWrite) -> CheckWrite<'_> {
        CheckWrite {
            generation: 0,
            at_ms: lodestore_server::db::now_ms(),
            now_ms: lodestore_server::db::now_ms(),
            checks_json: "[]",
            agent_key_state: "verified",
            guarantee: Some(g),
            daily: false,
            host_key_mismatch: None,
        }
    }
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let id = ts.create_local_repository("raced").await;
        let decided = || GuaranteeWrite {
            level: "create_only_enforced".into(),
            notes_json: "[]".into(),
            accepted: "create_only_enforced".into(),
            read_accepted: "overwrite_keeps_old_version".into(),
        };

        accept_now(&ts, &id, "host_can_overwrite_or_delete").await;
        let stored = update_checks(ts.state(), &id, &write(&decided()))
            .await
            .unwrap();
        assert_eq!(stored, Stored::AcceptedChanged, "[{}]", backend.name());
        assert_eq!(accepted(&ts, &id).await, "host_can_overwrite_or_delete");

        let key = AccessKey {
            access_key_id: "new".into(),
            secret_access_key: "new-secret".into(),
        };
        let stored = replace_agent_creds(ts.state(), &id, &key, "verified", "[]", &decided())
            .await
            .unwrap();
        assert!(!stored, "[{}]", backend.name());
        assert_eq!(accepted(&ts, &id).await, "host_can_overwrite_or_delete");

        accept_now(&ts, &id, "overwrite_keeps_old_version").await;
        assert_eq!(
            update_checks(ts.state(), &id, &write(&decided()))
                .await
                .unwrap(),
            Stored::Written
        );
        assert_eq!(accepted(&ts, &id).await, "create_only_enforced");
        ts.stop().await;
    }
}

/// The host key mismatch of a probe is written together with the check, under
/// the access generation it was measured with and only when no newer check
/// was stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_key_mismatch_is_written_with_its_check() {
    use lodestore_format::store::probe::HostKeyChange;
    use lodestore_server::repos::db::{CheckWrite, Stored, mismatch_update, update_checks};
    let change = HostKeyChange {
        pinned: "SHA256:old".into(),
        offered: "SHA256:new".into(),
    };
    let write = |generation: i64, at_ms: i64, mismatch: Option<Option<String>>| CheckWrite {
        generation,
        at_ms,
        now_ms: lodestore_server::db::now_ms(),
        checks_json: "[]",
        agent_key_state: "unchecked",
        guarantee: None,
        daily: false,
        host_key_mismatch: mismatch,
    };
    let now = lodestore_server::db::now_ms();
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let id = ts.create_local_repository("keyed").await;
        let column = || async {
            sqlx::query("SELECT host_key_mismatch_json FROM repositories WHERE id = $1")
                .bind(&id)
                .fetch_one(&ts.state().db.pool)
                .await
                .unwrap()
                .try_get::<Option<String>, _>("host_key_mismatch_json")
                .unwrap()
        };
        let store = |w: CheckWrite<'static>| {
            let (state, id) = (ts.state(), id.clone());
            async move { update_checks(state, &id, &w).await.unwrap() }
        };
        sqlx::query(
            "UPDATE repositories SET access_generation = 1, checked_at = NULL WHERE id = $1",
        )
        .bind(&id)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
        let seen = mismatch_update(Some(&change), false);

        assert_eq!(
            store(write(0, now, seen.clone())).await,
            Stored::Stale,
            "[{}]",
            backend.name()
        );
        assert_eq!(column().await, None);

        assert_eq!(store(write(1, now, seen.clone())).await, Stored::Written);
        let json: Value = serde_json::from_str(&column().await.unwrap()).unwrap();
        assert_eq!(
            json,
            json!({ "pinned": "SHA256:old", "offered": "SHA256:new" })
        );

        let cleared = mismatch_update(None, true);
        assert_eq!(cleared, Some(None));
        assert_eq!(
            store(write(1, now - 1, cleared.clone())).await,
            Stored::Stale
        );
        assert!(column().await.is_some(), "an older probe cleared it");
        assert_eq!(
            store(write(1, now + 1, mismatch_update(None, false))).await,
            Stored::Written
        );
        assert!(column().await.is_some());
        assert_eq!(store(write(1, now + 2, cleared)).await, Stored::Written);
        assert_eq!(column().await, None);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn access_replacements_survive_concurrent_writes() {
    use lodestore_server::repos::db::{
        AccessKey, GuaranteeWrite, Stored, replace_agent_creds, replace_host_key,
    };
    let ts = TestServer::start(&Backend::Sqlite).await;
    let id = ts.create_local_repository("busy").await;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writers: Vec<_> = (0..2)
        .map(|_| {
            let (pool, stop) = (ts.state().db.pool.clone(), stop.clone());
            tokio::spawn(async move {
                let mut n = 0i64;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    n += 1;
                    sqlx::query(
                        "INSERT INTO settings (key, value) VALUES ('busy-test', $1) \
                         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                    )
                    .bind(n.to_string())
                    .execute(&pool)
                    .await
                    .unwrap();
                }
            })
        })
        .collect();
    let key = AccessKey {
        access_key_id: "a".into(),
        secret_access_key: "b".into(),
    };
    for round in 0..1000 {
        let row = sqlx::query(
            "SELECT access_generation, guarantee_accepted FROM repositories WHERE id = $1",
        )
        .bind(&id)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap();
        let accepted: String = row.get("guarantee_accepted");
        let guarantee = GuaranteeWrite {
            level: accepted.clone(),
            notes_json: "[]".into(),
            accepted: accepted.clone(),
            read_accepted: accepted,
        };
        let generation: i64 = row.get("access_generation");
        let pinned = replace_host_key(
            ts.state(),
            &id,
            generation,
            "ssh-ed25519 AAAA",
            "unchecked",
            "[]",
            &guarantee,
        )
        .await
        .unwrap_or_else(|e| panic!("round {round}: {e:#}"));
        assert_eq!(pinned, Stored::Written);
        assert!(
            replace_agent_creds(ts.state(), &id, &key, "unchecked", "[]", &guarantee)
                .await
                .unwrap_or_else(|e| panic!("round {round}: {e:#}"))
        );
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for writer in writers {
        writer.await.unwrap();
    }
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sas_vending_material_carries_a_sas_and_no_account_key() {
    use lodestore_format::crypto::Entropy;
    use lodestore_format::repo::{InitOptions, PendingInit};
    use lodestore_server::repos::db::{
        NewRepository, RepoExtras, RepoLocation, create_repository_ext, record_sas_issued,
        writer_material,
    };
    use std::collections::BTreeMap;

    let account_key = data_encoding::BASE64.encode(&[9u8; 64]);
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let pending = PendingInit::generate(&mut Entropy::os(), InitOptions::default())
            .expect("generating a repository");
        let keys = pending.read_keys();
        let id = create_repository_ext(
            ts.state(),
            NewRepository {
                name: "vended".into(),
                location: RepoLocation {
                    backend: "azblob".into(),
                    options: BTreeMap::from([
                        ("account_name".into(), "lodeacct".into()),
                        ("endpoint".into(), "http://127.0.0.1:9/lodeacct".into()),
                        ("container".into(), "vended".into()),
                    ]),
                },
                format_repo_id: *keys.repo_id(),
                config_hash: lodestore_format::config::config_hash(pending.config_bytes()),
                master: keys.master().clone(),
                server_slot_secret: None,
                server_creds: None,
                agent_creds: None,
            },
            RepoExtras {
                secret_options: BTreeMap::from([("account_key".into(), account_key.clone())]),
                agent_sas_vending: true,
                ..RepoExtras::default()
            },
        )
        .await
        .unwrap();

        let material = writer_material(ts.state(), &id).await.unwrap();
        let options = &material.location.options;
        let sas = &options["sas_token"];
        assert!(sas.contains("sp=rcl"), "{sas}");
        assert!(!options.contains_key("account_key"), "{options:?}");
        let left = material.sas_expires_at_ms.unwrap() - lodestore_server::db::now_ms();
        let minute = 60_000;
        assert!(
            (23 * 60 * minute + 44 * minute..=23 * 60 * minute + 46 * minute).contains(&left),
            "[{}] {left}",
            backend.name()
        );
        let shown = format!("{:?}", material.location);
        let sig = sas
            .split('&')
            .find_map(|p| p.strip_prefix("sig="))
            .expect("a signature");
        assert!(
            !shown.contains(&account_key) && !shown.contains(sig),
            "{shown}"
        );

        sqlx::query(
            "INSERT INTO agents (id, hostname, repository_id, labels_json, os, arch, version, \
             status, enrolled_at, cert_serial, cert_issued_at, cert_not_after) \
             VALUES ('host-1', 'h', $1, '[]', 'linux', 'x86_64', '0.1.0', 'active', 1, 's', 1, 2)",
        )
        .bind(&id)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
        let stored = || async {
            sqlx::query("SELECT sas_expires_at FROM agents WHERE id = 'host-1'")
                .fetch_one(&ts.state().db.pool)
                .await
                .unwrap()
                .get::<Option<i64>, _>(0)
        };
        assert_eq!(stored().await, None);
        record_sas_issued(ts.state(), "host-1", 2000).await.unwrap();
        record_sas_issued(ts.state(), "host-1", 1000).await.unwrap();
        assert_eq!(stored().await, Some(2000));
        ts.stop().await;
    }
}
