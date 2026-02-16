use anyhow::{Context, Result};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use std::str::FromStr;
use tracing::info;

use crate::config::DbConfig;

/// Migrations embedded at compile time from `crates/gator-db/migrations/`.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// Create a connection pool with sensible defaults.
///
/// Configures WAL journal mode for better concurrent read/write performance
/// and sets a busy timeout so writers wait instead of failing immediately.
pub async fn create_pool(config: &DbConfig) -> Result<SqlitePool> {
    let url = config.database_url();
    let options = SqliteConnectOptions::from_str(&url)
        .with_context(|| format!("invalid database URL: {}", url))?
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(std::time::Duration::from_secs(5))
        .create_if_missing(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .with_context(|| format!("failed to connect to database at {}", url))?;
    Ok(pool)
}

/// Run all pending embedded migrations against the pool.
pub async fn run_migrations(pool: &SqlitePool) -> Result<()> {
    MIGRATOR
        .run(pool)
        .await
        .context("failed to run database migrations")?;

    info!("migrations applied successfully");
    Ok(())
}
