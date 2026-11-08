//! A compromised server queues restore jobs straight in its own database,
//! bypassing the REST checks. The agent must still refuse to write outside
//! the folders its own `agent.yaml` allows and must not restore in place
//! unless the host turned that on. Only public paths of the first release are
//! used (REST, the agent's gRPC session, SQL and the proto types), so the
//! scenarios run unchanged against that release.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use common::{AgentEnv, backends};
use data_encoding::BASE64URL_NOPAD;
use lodestore_proto::agent::v1::{ConflictPolicy, RestoreSpec};
use lodestore_server::db::now_ms;
use prost::Message;
use serde_json::{Value, json};
use sqlx::Row;

fn reset_agent_yaml(env: &AgentEnv) {
    let yaml = env.paths.agent_yaml();
    fs::write(&yaml, "{}\n").unwrap();
    fs::set_permissions(&yaml, fs::Permissions::from_mode(0o600)).unwrap();
}

async fn plant_restore(
    env: &AgentEnv,
    snapshot: &str,
    data: &Path,
    destination: &str,
    in_place: bool,
) -> Value {
    let pool = &env.ts().state().db.pool;
    let row = sqlx::query("SELECT repository_id, host_id FROM snapshots WHERE id = $1")
        .bind(snapshot)
        .fetch_one(pool)
        .await
        .unwrap();
    let repository: String = row.try_get("repository_id").unwrap();
    let host: Option<String> = row.try_get("host_id").unwrap();
    let record = json!({
        "snapshot_id": snapshot,
        "source_repository_id": repository,
        "source_host_id": host,
        "source_hostname": env.hostname,
        "items_b64": [BASE64URL_NOPAD.encode(data.to_string_lossy().as_bytes())],
        "destination": destination,
        "in_place": in_place,
        "conflict": "overwrite_changed",
        "restore_owners": false,
        "started_by": "admin",
    });
    let spec = RestoreSpec {
        snapshot_id: snapshot.to_string(),
        destination: destination.to_string(),
        in_place,
        conflict: ConflictPolicy::OverwriteChanged as i32,
        ..Default::default()
    };
    let job = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO jobs (id, host_id, source, state, spec, queued_at, kind, restore_json) \
         VALUES ($1, $2, 'manual', 'queued', $3, $4, 'restore', $5)",
    )
    .bind(&job)
    .bind(&env.host)
    .bind(spec.encode_to_vec())
    .bind(now_ms())
    .bind(record.to_string())
    .execute(pool)
    .await
    .unwrap();
    env.wait_job(&job).await
}

fn data_dir(env: &AgentEnv) -> PathBuf {
    let dir = env.work.path().join("data");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("file.txt"), b"backed up").unwrap();
    dir.canonicalize().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_server_cannot_restore_outside_roots() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let data = data_dir(&env);
        let snapshot = env.backup(&[&data]).await;
        reset_agent_yaml(&env);

        let planted = env.paths.config_dir.join("planted");
        let job = plant_restore(&env, &snapshot, &data, &planted.to_string_lossy(), false).await;

        assert_eq!(job["state"], "failed", "[{}] {job}", backend.name());
        assert_eq!(job["error_code"], "restore_outside_roots", "{job}");
        assert!(!planted.exists(), "[{}]", backend.name());
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_server_cannot_restore_in_place_by_default() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let data = data_dir(&env);
        let snapshot = env.backup(&[&data]).await;
        let file = data.join("file.txt");
        fs::write(&file, b"changed after the backup").unwrap();
        reset_agent_yaml(&env);

        let job = plant_restore(&env, &snapshot, &data, "/", true).await;

        assert_eq!(job["state"], "failed", "[{}] {job}", backend.name());
        assert_eq!(job["error_code"], "restore_in_place_disabled", "{job}");
        assert_eq!(
            fs::read(&file).unwrap(),
            b"changed after the backup",
            "[{}]",
            backend.name()
        );
        env.stop().await;
    }
}
