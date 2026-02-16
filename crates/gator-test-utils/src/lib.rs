use std::path::PathBuf;

use gator_db::{config::DbConfig, pool};
use sqlx::SqlitePool;

/// Create a temporary SQLite database with migrations applied.
///
/// Returns `(pool, db_path)`. Each test gets its own isolated database file
/// in the system temp directory. Call [`drop_test_db`] with the returned
/// path when the test is done.
pub async fn create_test_db() -> (SqlitePool, PathBuf) {
    let tmp_dir = std::env::temp_dir();
    let db_name = format!("gator_test_{}.db", uuid::Uuid::new_v4().simple());
    let db_path = tmp_dir.join(&db_name);

    let config = DbConfig::new(db_path.clone());
    let pool = pool::create_pool(&config)
        .await
        .expect("failed to create test database pool");

    pool::run_migrations(&pool)
        .await
        .expect("migrations should succeed on test database");

    (pool, db_path)
}

/// Clean up a temporary test database.
///
/// Closes connections implicitly when the pool is dropped, then removes the file.
pub async fn drop_test_db(db_path: &std::path::Path) {
    // SQLite WAL mode creates -wal and -shm sidecar files.
    let wal = db_path.with_extension("db-wal");
    let shm = db_path.with_extension("db-shm");
    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(wal);
    let _ = std::fs::remove_file(shm);
}
