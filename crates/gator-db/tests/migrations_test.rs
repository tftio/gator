//! Integration tests for database migrations and connection pooling.
//!
//! Each test creates a unique temporary database inside a shared containerized
//! PostgreSQL instance (via testcontainers), runs migrations, and drops it on
//! completion so tests are fully isolated and idempotent.

use sqlx::Row;
use uuid::Uuid;

use gator_db::config::DbConfig;
use gator_db::pool;

use gator_test_utils::{create_test_db, drop_test_db, pg_url};

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
    let (temp_pool, db_name) = create_test_db().await;

    pool::run_migrations(&temp_pool)
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
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn migrations_are_idempotent() {
    let (temp_pool, db_name) = create_test_db().await;

    // Run migrations twice -- second run should be a no-op.
    pool::run_migrations(&temp_pool)
        .await
        .expect("first migration run should succeed");

    pool::run_migrations(&temp_pool)
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
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn pool_creates_and_destroys_cleanly() {
    let (temp_pool, db_name) = create_test_db().await;

    pool::run_migrations(&temp_pool)
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

    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn table_counts_returns_expected_tables() {
    let (temp_pool, db_name) = create_test_db().await;

    pool::run_migrations(&temp_pool)
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
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn ensure_database_exists_is_idempotent() {
    // Create a unique database name via ensure_database_exists.
    let base_url = pg_url().await;
    let db_name = format!("gator_test_{}", Uuid::new_v4().simple());
    let url = format!("{base_url}/{db_name}");
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
    drop_test_db(&db_name).await;
}
