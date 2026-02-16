use anyhow::{Context, Result};
use sqlx::SqlitePool;
use sqlx::sqlite::SqlitePoolOptions;
use tracing::info;

use crate::config::DbConfig;

/// Migrations embedded at compile time from `crates/gator-db/migrations/`.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// Create a connection pool with sensible defaults.
pub async fn create_pool(config: &DbConfig) -> Result<SqlitePool> {
    let url = config.database_url();
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect(&url)
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
