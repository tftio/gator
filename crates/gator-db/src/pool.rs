use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgPool};
use tracing::info;

use crate::config::DbConfig;

/// Migrations embedded at compile time from `crates/gator-db/migrations/`.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// Create a connection pool with sensible defaults.
pub async fn create_pool(config: &DbConfig) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&config.database_url)
        .await
        .with_context(|| format!("failed to connect to database at {}", config.database_url))?;
    Ok(pool)
}

/// Run all pending embedded migrations against the pool.
pub async fn run_migrations(pool: &PgPool) -> Result<()> {
    MIGRATOR
        .run(pool)
        .await
        .context("failed to run database migrations")?;

    info!("migrations applied successfully");
    Ok(())
}

/// Ensure the target database exists, creating it if necessary.
///
/// Connects to the `postgres` maintenance database and issues
/// `CREATE DATABASE <name>` when the target database is absent.
pub async fn ensure_database_exists(config: &DbConfig) -> Result<()> {
    let db_name = config
        .database_name()
        .context("could not determine database name from URL")?;

    let maintenance_url = config.maintenance_url();

    let maint_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&maintenance_url)
        .await
        .with_context(|| {
            format!(
                "failed to connect to maintenance database at {}",
                maintenance_url
            )
        })?;

    // Check whether the database already exists.
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(db_name)
            .fetch_one(&maint_pool)
            .await
            .context("failed to query pg_database")?;

    if exists {
        info!(db = db_name, "database already exists");
    } else {
        // Database names cannot be parameterised in CREATE DATABASE, so we
        // validate the name to avoid SQL injection, then use string formatting.
        if !db_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            anyhow::bail!("database name {:?} contains invalid characters", db_name);
        }
        let stmt = format!("CREATE DATABASE {db_name}");
        maint_pool
            .execute(stmt.as_str())
            .await
            .with_context(|| format!("failed to create database {db_name}"))?;
        info!(db = db_name, "database created");
    }

    maint_pool.close().await;
    Ok(())
}

/// Return the row count for every user-defined table in the `public` schema.
///
/// Useful for the `gator init` success message.
pub async fn table_counts(pool: &PgPool) -> Result<Vec<(String, i64)>> {
    // Query information_schema for all tables, then count each one.
    let tables: Vec<(String,)> = sqlx::query_as(
        "SELECT tablename::text \
         FROM pg_tables \
         WHERE schemaname = 'public' \
         ORDER BY tablename",
    )
    .fetch_all(pool)
    .await
    .context("failed to list tables")?;

    let mut counts = Vec::with_capacity(tables.len());
    for (table_name,) in &tables {
        // Table names come from pg_tables so they are safe identifiers.
        let query = format!("SELECT COUNT(*) FROM {table_name}");
        let count: (i64,) = sqlx::query_as(&query)
            .fetch_one(pool)
            .await
            .with_context(|| format!("failed to count rows in {table_name}"))?;
        counts.push((table_name.clone(), count.0));
    }
    Ok(counts)
}
