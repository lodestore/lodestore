mod common;

use common::{Backend, backends, create_postgres_database};
use data_encoding::HEXLOWER;
use lodestore_server::config::{Config, Secret};
use lodestore_server::db::Db;
use lodestore_server::secrets::Sealer;
use serde_json::Value;
use sqlx::Row;
use tempfile::TempDir;

const FIXTURE: &str = include_str!("fixtures/secrets-v1.json");

struct Fixture {
    unlock_key: String,
    rows: Vec<FixtureRow>,
}

struct FixtureRow {
    id: String,
    kind: String,
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
    plaintext: Vec<u8>,
}

fn fixture() -> Fixture {
    let json: Value = serde_json::from_str(FIXTURE).unwrap();
    let hex = |v: &Value| HEXLOWER.decode(v.as_str().unwrap().as_bytes()).unwrap();
    let text = |v: &Value| v.as_str().unwrap().to_owned();
    Fixture {
        unlock_key: text(&json["unlock_key"]),
        rows: json["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                assert_eq!(r["format"], 1);
                FixtureRow {
                    id: text(&r["id"]),
                    kind: text(&r["kind"]),
                    nonce: hex(&r["nonce"]),
                    ciphertext: hex(&r["ciphertext"]),
                    plaintext: hex(&r["plaintext"]),
                }
            })
            .collect(),
    }
}

async fn insert_row(db: &Db, id: &str, kind: &str, format: i64, nonce: &[u8], ciphertext: &[u8]) {
    sqlx::query(
        "INSERT INTO secrets (id, kind, format, nonce, ciphertext, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(kind)
    .bind(format)
    .bind(nonce.to_vec())
    .bind(ciphertext.to_vec())
    .bind(1_700_000_000_000_i64)
    .execute(&db.pool)
    .await
    .unwrap();
}

async fn database(backend: &Backend, fx: &Fixture) -> (Db, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let url = match backend {
        Backend::Sqlite => format!("sqlite://{}/lodestore.db?mode=rwc", dir.path().display()),
        Backend::Postgres(admin) => create_postgres_database(admin).await,
    };
    let db = Db::connect(&url).await.unwrap();
    for row in &fx.rows {
        insert_row(&db, &row.id, &row.kind, 1, &row.nonce, &row.ciphertext).await;
    }
    (db, dir)
}

fn config(dir: &TempDir, unlock_key: &str) -> Config {
    Config {
        unlock_key: Some(Secret::new(unlock_key)),
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    }
}

async fn snapshot(db: &Db) -> Vec<(String, i64, Vec<u8>, Vec<u8>)> {
    sqlx::query("SELECT id, format, nonce, ciphertext FROM secrets ORDER BY id")
        .fetch_all(&db.pool)
        .await
        .unwrap()
        .iter()
        .map(|r| {
            (
                r.get("id"),
                r.get("format"),
                r.get("nonce"),
                r.get("ciphertext"),
            )
        })
        .collect()
}

async fn count_format(db: &Db, format: i64) -> i64 {
    sqlx::query("SELECT COUNT(*) AS n FROM secrets WHERE format = $1")
        .bind(format)
        .fetch_one(&db.pool)
        .await
        .unwrap()
        .get("n")
}

#[tokio::test]
async fn secrets_v1_database_reads_after_upgrade() {
    let fx = fixture();
    for backend in backends() {
        let (db, dir) = database(&backend, &fx).await;
        assert_eq!(count_format(&db, 1).await, fx.rows.len() as i64);

        let sealer = Sealer::load(&db, &config(&dir, &fx.unlock_key))
            .await
            .unwrap();

        for row in &fx.rows {
            let got = sealer.get_secret(&row.id).await.unwrap();
            assert_eq!(*got, row.plaintext, "{} {}", backend.name(), row.kind);
        }
        assert_eq!(count_format(&db, 1).await, 0, "{}", backend.name());
        assert_eq!(count_format(&db, 2).await, fx.rows.len() as i64);
    }
}

#[tokio::test]
async fn secrets_migration_is_idempotent() {
    let fx = fixture();
    for backend in backends() {
        let (db, dir) = database(&backend, &fx).await;
        let cfg = config(&dir, &fx.unlock_key);
        Sealer::load(&db, &cfg).await.unwrap();
        let first = snapshot(&db).await;

        let sealer = Sealer::load(&db, &cfg).await.unwrap();
        assert_eq!(snapshot(&db).await, first, "{}", backend.name());
        let row = &fx.rows[1];
        assert_eq!(*sealer.get_secret(&row.id).await.unwrap(), row.plaintext);
    }
}

#[tokio::test]
async fn secrets_wrong_unlock_key_changes_nothing() {
    let fx = fixture();
    for backend in backends() {
        let (db, dir) = database(&backend, &fx).await;
        let before = snapshot(&db).await;

        let wrong = HEXLOWER.encode(&[9u8; 32]);
        let err = Sealer::load(&db, &config(&dir, &wrong))
            .await
            .err()
            .expect("a wrong unlock key must stop startup");
        assert!(
            err.to_string().contains("does not match this database"),
            "{err}"
        );
        assert_eq!(snapshot(&db).await, before, "{}", backend.name());
        assert_eq!(count_format(&db, 1).await, fx.rows.len() as i64);
    }
}

#[tokio::test]
async fn secrets_migration_failure_rolls_back_every_row() {
    let fx = fixture();
    for backend in backends() {
        let (db, dir) = database(&backend, &fx).await;
        insert_row(
            &db,
            "zzz-corrupt",
            "channel-secret",
            1,
            &[0u8; 24],
            &[1u8; 40],
        )
        .await;
        let before = snapshot(&db).await;

        let err = Sealer::load(&db, &config(&dir, &fx.unlock_key))
            .await
            .err()
            .expect("a row that does not open must stop startup");
        assert!(err.to_string().contains("zzz-corrupt"), "{err:#}");
        assert_eq!(snapshot(&db).await, before, "{}", backend.name());
    }
}

#[tokio::test]
async fn secrets_unknown_format_is_refused_and_left_alone() {
    let fx = fixture();
    for backend in backends() {
        let (db, dir) = database(&backend, &fx).await;
        insert_row(
            &db,
            "future-row",
            "channel-secret",
            3,
            &[0u8; 24],
            &[1u8; 40],
        )
        .await;

        let sealer = Sealer::load(&db, &config(&dir, &fx.unlock_key))
            .await
            .unwrap();
        let err = sealer.get_secret("future-row").await.unwrap_err();
        assert!(err.to_string().contains("unknown format 3"), "{err}");
        assert_eq!(count_format(&db, 3).await, 1, "{}", backend.name());
    }
}
