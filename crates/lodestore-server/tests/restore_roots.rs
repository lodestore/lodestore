mod common;

use common::{AgentEnv, WAIT, backends, bearer_client, wait_until};
use data_encoding::BASE64URL_NOPAD;
use serde_json::{Value, json};
use sqlx::Row;

struct Seeded {
    snapshot: String,
    item: String,
}

async fn seed(env: &AgentEnv) -> Seeded {
    let dir = env.work.path().join("src");
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    std::fs::write(dir.join("a.txt"), b"alpha").unwrap();
    let snapshot = env.backup(&[&dir]).await;
    let verified = wait_until(WAIT, || async {
        sqlx::query("SELECT verified_at FROM snapshots WHERE id = $1")
            .bind(&snapshot)
            .fetch_one(&env.ts().state().db.pool)
            .await
            .is_ok_and(|row| {
                row.try_get::<Option<i64>, _>("verified_at")
                    .unwrap()
                    .is_some()
            })
    })
    .await;
    assert!(verified, "the agent signs the snapshot");
    Seeded {
        snapshot,
        item: BASE64URL_NOPAD.encode(dir.to_string_lossy().as_bytes()),
    }
}

fn body(env: &AgentEnv, seeded: &Seeded, destination: &str, in_place: bool) -> Value {
    json!({
        "snapshot_id": seeded.snapshot,
        "items": [seeded.item],
        "target_host_id": env.host,
        "destination": destination,
        "in_place": in_place,
        "conflict": "skip_existing",
        "restore_owners": false,
        "accept_unverified": true,
    })
}

async fn facts(env: &AgentEnv) -> Option<Value> {
    let row = sqlx::query("SELECT facts_json FROM agents WHERE id = $1")
        .bind(&env.host)
        .fetch_one(&env.ts().state().db.pool)
        .await
        .unwrap();
    let text: Option<String> = row.try_get("facts_json").unwrap();
    text.map(|t| serde_json::from_str(&t).unwrap())
}

async fn set_facts(env: &AgentEnv, facts: Option<Value>) {
    sqlx::query("UPDATE agents SET facts_json = $1 WHERE id = $2")
        .bind(facts.map(|f| f.to_string()))
        .bind(&env.host)
        .execute(&env.ts().state().db.pool)
        .await
        .unwrap();
}

async fn post_session(env: &AgentEnv, payload: &Value) -> (u16, Value) {
    let response = env.api.post("/api/v1/restores", payload).await;
    (
        response.status().as_u16(),
        response.json().await.unwrap_or(Value::Null),
    )
}

async fn post_token(env: &AgentEnv, client: &reqwest::Client, payload: &Value) -> (u16, Value) {
    let response = client
        .post(format!("{}/api/v1/restores", env.ts().rest_url()))
        .json(payload)
        .send()
        .await
        .expect("request");
    (
        response.status().as_u16(),
        response.json().await.unwrap_or(Value::Null),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_roots_facts_stored_from_heartbeat() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let stored = wait_until(WAIT, || async {
            facts(&env)
                .await
                .is_some_and(|f| f.get("restore").is_some())
        })
        .await;
        assert!(stored, "[{}]", backend.name());
        let restore = facts(&env).await.unwrap()["restore"].clone();
        assert_eq!(
            restore["roots"],
            json!([env.work.path().to_string_lossy()]),
            "[{}]",
            backend.name()
        );
        assert_eq!(restore["allow_in_place"], true);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_roots_outside_refused_early() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let seeded = seed(&env).await;
        assert!(
            wait_until(WAIT, || async {
                facts(&env)
                    .await
                    .is_some_and(|f| f.get("restore").is_some())
            })
            .await
        );
        let work = env.work.path().display().to_string();
        let token = env.ts().issue_token(&["restore:host"]).await;
        let client = bearer_client(&token);
        let label = backend.name();

        let outside = [
            format!("{work}x/out"),
            "/srv/elsewhere".to_string(),
            "/".to_string() + "tmp",
        ];
        for destination in &outside {
            let payload = body(&env, &seeded, destination, false);
            let (status, response) = post_session(&env, &payload).await;
            assert_eq!(status, 400, "session {destination} [{label}] {response}");
            assert_eq!(response["code"], "restore_outside_roots", "{response}");
            assert!(
                response["message"].as_str().unwrap().contains(&work),
                "{response}"
            );
            let (status, response) = post_token(&env, &client, &payload).await;
            assert_eq!(status, 400, "token {destination} [{label}] {response}");
            assert_eq!(response["code"], "restore_outside_roots", "{response}");
        }

        let inside = format!("{work}/out/x");
        let payload = body(&env, &seeded, &inside, false);
        assert_eq!(post_session(&env, &payload).await.0, 201, "[{label}]");
        assert_eq!(
            post_token(&env, &client, &payload).await.0,
            201,
            "[{label}]"
        );
        let payload = body(&env, &seeded, &work, false);
        assert_eq!(post_session(&env, &payload).await.0, 201, "[{label}]");
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_roots_in_place_disabled_refused_early() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let seeded = seed(&env).await;
        let work = env.work.path().display().to_string();
        set_facts(
            &env,
            Some(json!({ "restore": { "roots": [work], "allow_in_place": false } })),
        )
        .await;
        let payload = body(&env, &seeded, "/", true);
        let (status, response) = post_session(&env, &payload).await;
        assert_eq!(status, 400, "[{}] {response}", backend.name());
        assert_eq!(response["code"], "restore_in_place_disabled", "{response}");
        let message = response["message"].as_str().unwrap();
        assert!(
            message.contains("restore.allow_in_place: true"),
            "{message}"
        );
        assert!(message.contains("--allow-in-place-restore"), "{message}");
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_roots_unknown_facts_pass() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let seeded = seed(&env).await;
        let payload = body(&env, &seeded, "/srv/elsewhere", false);
        for unknown in [None, Some(json!({ "uptime_s": 1 }))] {
            set_facts(&env, unknown).await;
            let (status, response) = post_session(&env, &payload).await;
            assert_eq!(status, 201, "[{}] {response}", backend.name());
        }
        env.stop().await;
    }
}

async fn listed_host(env: &AgentEnv) -> Value {
    let hosts: Vec<Value> = env
        .api
        .get("/api/v1/restore/hosts")
        .await
        .json()
        .await
        .unwrap();
    hosts
        .into_iter()
        .find(|h| h["host_id"] == env.host)
        .expect("the host is listed")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_roots_listed_for_restore_hosts() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let stored = wait_until(WAIT, || async {
            facts(&env)
                .await
                .is_some_and(|f| f.get("restore").is_some())
        })
        .await;
        assert!(stored, "[{}]", backend.name());
        let host = listed_host(&env).await;
        assert_eq!(
            host["restore_roots"],
            json!([env.work.path().to_string_lossy()])
        );
        assert_eq!(host["allow_in_place"], true);

        set_facts(&env, None).await;
        let host = listed_host(&env).await;
        assert_eq!(host["restore_roots"], json!([]));
        assert!(host["allow_in_place"].is_null());
        env.stop().await;
    }
}
