//! Integration tests for database migrations and connection pooling.
//!
//! Each test creates a unique temporary SQLite database file, runs migrations,
//! and removes it on completion so tests are fully isolated and idempotent.

use sqlx::Row;

use gator_db::pool;

use gator_test_utils::{create_test_db, drop_test_db};

/// Expected tables created by the migrations.
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
    let (temp_pool, db_path) = create_test_db().await;

    pool::run_migrations(&temp_pool)
        .await
        .expect("migrations should succeed");

    // Verify all expected tables exist via sqlite_master.
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT name FROM sqlite_master \
         WHERE type='table' AND name NOT LIKE '_sqlx%' AND name != 'sqlite_sequence' \
         ORDER BY name",
    )
    .fetch_all(&temp_pool)
    .await
    .expect("should list tables");

    let table_names: Vec<&str> = rows.iter().map(|(name,)| name.as_str()).collect();

    assert_eq!(
        table_names, EXPECTED_TABLES,
        "migration should create exactly the expected tables"
    );

    temp_pool.close().await;
    drop_test_db(&db_path).await;
}

#[tokio::test]
async fn migrations_are_idempotent() {
    let (temp_pool, db_path) = create_test_db().await;

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
    drop_test_db(&db_path).await;
}

#[tokio::test]
async fn pool_creates_and_destroys_cleanly() {
    let (temp_pool, db_path) = create_test_db().await;

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

    drop_test_db(&db_path).await;
}
