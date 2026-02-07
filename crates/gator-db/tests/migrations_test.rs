//! Integration tests for database migrations and connection pooling.
//!
//! These tests require a running PostgreSQL instance accessible via
//! `GATOR_DATABASE_URL` (or the default `postgresql://localhost:5432/gator`).
//!
//! Each test creates a unique temporary database, runs migrations, and drops
//! it on completion so tests are fully isolated and idempotent.

use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgPool, Row};
use uuid::Uuid;

use gator_db::config::DbConfig;
use gator_db::pool;

/// Helper: create a unique temporary database and return a pool pointing at it.
///
/// The database is named `gator_test_<uuid>` to avoid collisions.
async fn create_temp_db() -> (PgPool, String) {
    // Connect to the default maintenance DB to create the temp one.
    let base_config = DbConfig::from_env();
    let maint_url = base_config.maintenance_url();

    let maint_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&maint_url)
        .await
        .expect("failed to connect to maintenance database");

    let db_name = format!("gator_test_{}", Uuid::new_v4().simple());
    let stmt = format!("CREATE DATABASE {db_name}");
    maint_pool
        .execute(stmt.as_str())
        .await
        .unwrap_or_else(|e| panic!("failed to create temp database {db_name}: {e}"));
    maint_pool.close().await;

    // Build URL for the new temp database.
    let temp_url = match base_config.database_url.rfind('/') {
        Some(pos) => format!("{}/{db_name}", &base_config.database_url[..pos]),
        None => panic!("cannot parse database URL"),
    };

    let temp_pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&temp_url)
        .await
        .unwrap_or_else(|e| panic!("failed to connect to temp database {db_name}: {e}"));

    (temp_pool, db_name)
}

/// Helper: drop the temporary database.
async fn drop_temp_db(db_name: &str) {
    let base_config = DbConfig::from_env();
    let maint_url = base_config.maintenance_url();

    let maint_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&maint_url)
        .await
        .expect("failed to connect to maintenance database for cleanup");

    // Terminate existing connections first.
    let terminate = format!(
        "SELECT pg_terminate_backend(pid) \
         FROM pg_stat_activity \
         WHERE datname = '{db_name}' AND pid <> pg_backend_pid()"
    );
    let _ = maint_pool.execute(terminate.as_str()).await;

    let stmt = format!("DROP DATABASE IF EXISTS {db_name}");
    let _ = maint_pool.execute(stmt.as_str()).await;
    maint_pool.close().await;
}

/// Expected tables created by the initial migration.
const EXPECTED_TABLES: &[&str] = &[
    "agent_events",
    "gate_results",
    "invariants",
    "plans",
    "task_dependencies",
    "task_invariants",
    "tasks",
];

#[tokio::test]
async fn migrations_create_all_tables() {
    let (temp_pool, db_name) = create_temp_db().await;

    let migrations_path = pool::default_migrations_path();
    pool::run_migrations(&temp_pool, migrations_path)
        .await
        .expect("migrations should succeed");

    // Verify all expected tables exist.
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT tablename::text FROM pg_tables \
         WHERE schemaname = 'public' \
         ORDER BY tablename",
    )
    .fetch_all(&temp_pool)
    .await
    .expect("should list tables");

    let table_names: Vec<&str> = rows.iter().map(|(name,)| name.as_str()).collect();

    // Filter out the sqlx metadata table.
    let user_tables: Vec<&str> = table_names
        .iter()
        .filter(|t| !t.starts_with("_sqlx"))
        .copied()
        .collect();

    assert_eq!(
        user_tables, EXPECTED_TABLES,
        "migration should create exactly the expected tables"
    );

    temp_pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn migrations_are_idempotent() {
    let (temp_pool, db_name) = create_temp_db().await;
    let migrations_path = pool::default_migrations_path();

    // Run migrations twice -- second run should be a no-op.
    pool::run_migrations(&temp_pool, migrations_path)
        .await
        .expect("first migration run should succeed");

    pool::run_migrations(&temp_pool, migrations_path)
        .await
        .expect("second migration run should succeed (idempotent)");

    // Tables should still be present and empty.
    for table in EXPECTED_TABLES {
        let query = format!("SELECT COUNT(*) AS cnt FROM {table}");
        let row = sqlx::query(&query)
            .fetch_one(&temp_pool)
            .await
            .unwrap_or_else(|e| panic!("failed to count {table}: {e}"));
        let count: i64 = row.get("cnt");
        assert_eq!(count, 0, "table {table} should be empty after migrations");
    }

    temp_pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn pool_creates_and_destroys_cleanly() {
    let (temp_pool, db_name) = create_temp_db().await;

    let migrations_path = pool::default_migrations_path();
    pool::run_migrations(&temp_pool, migrations_path)
        .await
        .expect("migrations should succeed");

    // Verify pool is functional.
    let one: (i32,) = sqlx::query_as("SELECT 1")
        .fetch_one(&temp_pool)
        .await
        .expect("simple query should work");
    assert_eq!(one.0, 1);

    // Close the pool and verify it shuts down without error.
    temp_pool.close().await;

    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn table_counts_returns_expected_tables() {
    let (temp_pool, db_name) = create_temp_db().await;

    let migrations_path = pool::default_migrations_path();
    pool::run_migrations(&temp_pool, migrations_path)
        .await
        .expect("migrations should succeed");

    let counts = pool::table_counts(&temp_pool)
        .await
        .expect("table_counts should succeed");

    // Filter out sqlx metadata.
    let user_counts: Vec<(&str, i64)> = counts
        .iter()
        .filter(|(name, _)| !name.starts_with("_sqlx"))
        .map(|(name, count)| (name.as_str(), *count))
        .collect();

    assert_eq!(user_counts.len(), EXPECTED_TABLES.len());
    for (name, count) in &user_counts {
        assert_eq!(*count, 0, "table {name} should be empty");
    }

    temp_pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn ensure_database_exists_is_idempotent() {
    // Create a unique database name via ensure_database_exists.
    let db_name = format!("gator_test_{}", Uuid::new_v4().simple());
    let base_config = DbConfig::from_env();
    let url = match base_config.database_url.rfind('/') {
        Some(pos) => format!("{}/{db_name}", &base_config.database_url[..pos]),
        None => panic!("cannot parse database URL"),
    };

    let config = DbConfig::new(&url);

    // First call should create the database.
    pool::ensure_database_exists(&config)
        .await
        .expect("first ensure should succeed");

    // Second call should be a no-op (idempotent).
    pool::ensure_database_exists(&config)
        .await
        .expect("second ensure should succeed (idempotent)");

    // Clean up.
    drop_temp_db(&db_name).await;
}
