//! Shared test utilities for gator integration tests.
//!
//! Provides a PostgreSQL instance shared across tests. Each test gets its
//! own database within the instance.
//!
//! Two modes:
//! - **`GATOR_TEST_PG_URL`** set (nextest setup script): use the external
//!   container directly. No testcontainers overhead per process.
//! - **No env var** (`cargo test`): spin up a container via testcontainers,
//!   shared per binary through a `OnceCell`.

use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgPool};
use testcontainers::ContainerAsync;
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio::sync::OnceCell;
use uuid::Uuid;

use gator_db::pool;

/// Shared container state: base URL and optional container handle (kept alive).
struct SharedPg {
    base_url: String,
    /// Held to keep the container alive. `None` when using an external URL.
    _container: Option<ContainerAsync<Postgres>>,
}

/// Lazily-initialized shared PostgreSQL.
static SHARED_PG: OnceCell<SharedPg> = OnceCell::const_new();

async fn init_shared_pg() -> SharedPg {
    // If a setup script already started a container, use that directly.
    if let Ok(url) = std::env::var("GATOR_TEST_PG_URL") {
        return SharedPg {
            base_url: url,
            _container: None,
        };
    }

    let container = Postgres::default()
        .with_tag("18")
        .start()
        .await
        .expect("failed to start PostgreSQL container");

    let host = container.get_host().await.expect("failed to get host");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("failed to get mapped port");

    let base_url = format!("postgresql://postgres:postgres@{host}:{port}");

    SharedPg {
        base_url,
        _container: Some(container),
    }
}

/// Base URL for the shared PostgreSQL.
///
/// Lazily starts a container on first call (unless `GATOR_TEST_PG_URL` is
/// set). The URL points at the server root (no database name appended).
pub async fn pg_url() -> &'static str {
    let shared = SHARED_PG.get_or_init(init_shared_pg).await;
    &shared.base_url
}

/// Create a temporary database with migrations applied.
///
/// Returns `(pool, db_name)`. The pool connects to a uniquely-named
/// database within the shared instance. Call [`drop_test_db`] with the
/// returned `db_name` when the test is done.
pub async fn create_test_db() -> (PgPool, String) {
    let base_url = pg_url().await;

    // Connect to the default "postgres" database to issue CREATE DATABASE.
    let maint_url = format!("{base_url}/postgres");
    let maint_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(30))
        .connect(&maint_url)
        .await
        .expect("failed to connect to maintenance database in container");

    let db_name = format!("gator_test_{}", Uuid::new_v4().simple());
    let stmt = format!("CREATE DATABASE {db_name}");
    maint_pool
        .execute(stmt.as_str())
        .await
        .unwrap_or_else(|e| panic!("failed to create temp database {db_name}: {e}"));
    maint_pool.close().await;

    // Connect to the new database and run migrations.
    let temp_url = format!("{base_url}/{db_name}");
    let temp_pool = PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(30))
        .connect(&temp_url)
        .await
        .unwrap_or_else(|e| panic!("failed to connect to temp database {db_name}: {e}"));

    pool::run_migrations(&temp_pool)
        .await
        .expect("migrations should succeed");

    (temp_pool, db_name)
}

/// Drop a temporary database.
///
/// Terminates existing connections and drops the database. Safe to call
/// even if the database was already dropped.
pub async fn drop_test_db(db_name: &str) {
    let base_url = pg_url().await;
    let maint_url = format!("{base_url}/postgres");

    let maint_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(30))
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
