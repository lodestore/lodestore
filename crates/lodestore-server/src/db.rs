use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use sqlx::AnyPool;
use sqlx::any::{AnyPoolOptions, install_default_drivers};

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
        migrator
            .run(&pool)
            .await
            .context("applying database migrations")?;
        Ok(Db { pool, dialect })
    }
}
