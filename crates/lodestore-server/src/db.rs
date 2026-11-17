use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use sqlx::any::{AnyPoolOptions, install_default_drivers};
use sqlx::{AnyPool, Row};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Sqlite,
    Postgres,
}

#[derive(Clone)]
pub struct Db {
    pub pool: AnyPool,
    pub dialect: Dialect,
}

const REBIND_PENDING_SQL: &str = "SELECT COUNT(*) AS n FROM snapshots WHERE (\
    job_id IS NOT NULL AND EXISTS (\
        SELECT 1 FROM jobs j WHERE j.id = snapshots.job_id AND (\
            (j.policy_name IS NULL) <> (snapshots.policy_name IS NULL) \
            OR j.policy_name <> snapshots.policy_name \
            OR (j.stdin_name IS NULL) <> (snapshots.stdin_name IS NULL) \
            OR j.stdin_name <> snapshots.stdin_name \
            OR snapshots.agent_id_claim IS NULL OR snapshots.agent_id_claim <> snapshots.host_id))\
    ) OR (\
    job_id IS NOT NULL AND (origin IS NULL OR origin <> 'stdin') AND EXISTS (\
        SELECT 1 FROM jobs j WHERE j.id = snapshots.job_id AND j.source = 'cli'))";

pub async fn pending_policy_rebind(pool: &AnyPool) -> Option<i64> {
    let applied = sqlx::query("SELECT 1 FROM _sqlx_migrations WHERE version = 32")
        .fetch_optional(pool)
        .await
        .ok()?;
    if applied.is_some() {
        return None;
    }
    sqlx::query(REBIND_PENDING_SQL)
        .fetch_one(pool)
        .await
        .ok()?
        .try_get::<i64, _>("n")
        .ok()
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Db {
    pub async fn connect(url: &str) -> anyhow::Result<Db> {
        install_default_drivers();
        let dialect = if url.starts_with("sqlite:") {
            Dialect::Sqlite
        } else if url.starts_with("postgres:") || url.starts_with("postgresql:") {
            Dialect::Postgres
        } else {
            anyhow::bail!("unsupported database URL scheme: use sqlite: or postgres:");
        };
        let sqlite = dialect == Dialect::Sqlite;
        let pool = AnyPoolOptions::new()
            .max_connections(if sqlite { 4 } else { 10 })
            .after_connect(|conn, _| {
                Box::pin(async move {
                    if conn.backend_name() == "SQLite" {
                        sqlx::query("PRAGMA synchronous = NORMAL")
                            .execute(&mut *conn)
                            .await?;
                    }
                    Ok(())
                })
            })
            .connect(url)
            .await
            .context("connecting to the database")?;
        if sqlite {
            sqlx::raw_sql("PRAGMA journal_mode = WAL")
                .execute(&pool)
                .await
                .context("enabling WAL")?;
        }
        let migrator = if sqlite {
            sqlx::migrate!("./migrations/sqlite")
        } else {
            sqlx::migrate!("./migrations/postgres")
        };
        let rebind = pending_policy_rebind(&pool).await;
        migrator
            .run(&pool)
            .await
            .context("applying database migrations")?;
        if let Some(n) = rebind.filter(|n| *n > 0) {
            tracing::warn!(
                snapshots = n,
                "took catalog snapshots that disagreed with their job out of retention"
            );
        }
        Ok(Db { pool, dialect })
    }
}
