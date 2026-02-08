//! Tests for retry feedback in task materialization (T018).

use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgPool};
use uuid::Uuid;

use gator_db::models::{InvariantKind, InvariantScope};
use gator_db::queries::gate_results::{self, NewGateResult};
use gator_db::queries::invariants::{self, NewInvariant};
use gator_db::queries::plans as plan_db;
use gator_db::queries::tasks as task_db;

use gator_core::plan::materialize_task;

// ===========================================================================
// Test harness
// ===========================================================================

async fn create_temp_db() -> (PgPool, String) {
    let database_url =
        std::env::var("GATOR_DATABASE_URL").unwrap_or_else(|_| {
            "postgresql://localhost:5432/gator".to_string()
        });

    let maint_url = match database_url.rfind('/') {
        Some(pos) => format!("{}/postgres", &database_url[..pos]),
        None => panic!("cannot parse GATOR_DATABASE_URL"),
    };

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
        .unwrap_or_else(|e| panic!("failed to create temp database: {e}"));
    maint_pool.close().await;

    let temp_url = match database_url.rfind('/') {
        Some(pos) => format!("{}/{db_name}", &database_url[..pos]),
        None => panic!("cannot parse GATOR_DATABASE_URL"),
    };

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&temp_url)
        .await
        .unwrap_or_else(|e| panic!("failed to connect to temp db: {e}"));

    let migrations_path = gator_db::pool::default_migrations_path();
    gator_db::pool::run_migrations(&pool, migrations_path)
        .await
        .expect("migrations should succeed");

    (pool, db_name)
}

async fn drop_temp_db(db_name: &str) {
    let database_url =
        std::env::var("GATOR_DATABASE_URL").unwrap_or_else(|_| {
            "postgresql://localhost:5432/gator".to_string()
        });

    let maint_url = match database_url.rfind('/') {
        Some(pos) => format!("{}/postgres", &database_url[..pos]),
        None => return,
    };

    let maint_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&maint_url)
        .await
        .expect("failed to connect for cleanup");

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

/// Create a plan + task + invariant, returning (task_id, invariant_id).
async fn create_test_fixtures(pool: &PgPool) -> (Uuid, Uuid) {
    let plan = plan_db::insert_plan(pool, "retry-plan", "/tmp/test", "main", None, "claude-code", "worktree")
        .await
        .expect("insert plan");

    let task = task_db::insert_task(
        pool,
        plan.id,
        "retry-task",
        "A task that will be retried",
        "narrow",
        "auto",
        3,
        None,
    )
    .await
    .expect("insert task");

    let inv = invariants::insert_invariant(
        pool,
        &NewInvariant {
            name: "cargo-test",
            description: Some("Run cargo test"),
            kind: InvariantKind::TestSuite,
            command: "cargo",
            args: &["test".to_string()],
            expected_exit_code: 0,
            threshold: None,
            scope: InvariantScope::Project,
        },
    )
    .await
    .expect("insert invariant");

    task_db::link_task_invariant(pool, task.id, inv.id)
        .await
        .expect("link invariant");

    (task.id, inv.id)
}

// ===========================================================================
// Tests
// ===========================================================================

#[tokio::test]
async fn attempt_zero_has_no_feedback_section() {
    let (pool, db_name) = create_temp_db().await;
    let (task_id, _inv_id) = create_test_fixtures(&pool).await;

    // Task is at attempt 0 by default.
    let md = materialize_task(&pool, task_id)
        .await
        .expect("materialize should succeed");

    assert!(
        !md.contains("Previous Attempt Feedback"),
        "attempt 0 should not have feedback section"
    );

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn attempt_one_with_failures_includes_feedback() {
    let (pool, db_name) = create_temp_db().await;
    let (task_id, inv_id) = create_test_fixtures(&pool).await;

    // Record a failing gate result for attempt 0.
    gate_results::insert_gate_result(
        &pool,
        &NewGateResult {
            task_id,
            invariant_id: inv_id,
            attempt: 0,
            passed: false,
            exit_code: Some(1),
            stdout: Some("test output".to_string()),
            stderr: Some("error: test failed\n  at src/lib.rs:42".to_string()),
            duration_ms: Some(500),
        },
    )
    .await
    .expect("insert gate result");

    // Manually set attempt to 1 to simulate retry.
    sqlx::query("UPDATE tasks SET attempt = 1 WHERE id = $1")
        .bind(task_id)
        .execute(&pool)
        .await
        .expect("update attempt");

    let md = materialize_task(&pool, task_id)
        .await
        .expect("materialize should succeed");

    assert!(
        md.contains("Previous Attempt Feedback"),
        "attempt 1 should have feedback section"
    );
    assert!(
        md.contains("cargo-test"),
        "feedback should include invariant name"
    );
    assert!(
        md.contains("Exit code:** 1"),
        "feedback should include exit code"
    );
    assert!(
        md.contains("error: test failed"),
        "feedback should include stderr"
    );

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn attempt_one_with_all_passed_has_no_feedback() {
    let (pool, db_name) = create_temp_db().await;
    let (task_id, inv_id) = create_test_fixtures(&pool).await;

    // Record a passing gate result for attempt 0.
    gate_results::insert_gate_result(
        &pool,
        &NewGateResult {
            task_id,
            invariant_id: inv_id,
            attempt: 0,
            passed: true,
            exit_code: Some(0),
            stdout: Some("all tests passed".to_string()),
            stderr: None,
            duration_ms: Some(200),
        },
    )
    .await
    .expect("insert gate result");

    // Manually set attempt to 1.
    sqlx::query("UPDATE tasks SET attempt = 1 WHERE id = $1")
        .bind(task_id)
        .execute(&pool)
        .await
        .expect("update attempt");

    let md = materialize_task(&pool, task_id)
        .await
        .expect("materialize should succeed");

    assert!(
        !md.contains("Previous Attempt Feedback"),
        "should not have feedback when previous attempt all passed"
    );

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn stderr_truncated_at_2048_bytes() {
    let (pool, db_name) = create_temp_db().await;
    let (task_id, inv_id) = create_test_fixtures(&pool).await;

    // Create a long stderr (3000 bytes).
    let long_stderr = "x".repeat(3000);

    gate_results::insert_gate_result(
        &pool,
        &NewGateResult {
            task_id,
            invariant_id: inv_id,
            attempt: 0,
            passed: false,
            exit_code: Some(1),
            stdout: None,
            stderr: Some(long_stderr),
            duration_ms: Some(100),
        },
    )
    .await
    .expect("insert gate result");

    // Manually set attempt to 1.
    sqlx::query("UPDATE tasks SET attempt = 1 WHERE id = $1")
        .bind(task_id)
        .execute(&pool)
        .await
        .expect("update attempt");

    let md = materialize_task(&pool, task_id)
        .await
        .expect("materialize should succeed");

    assert!(
        md.contains("Previous Attempt Feedback"),
        "should have feedback section"
    );
    // The stderr in the markdown should be truncated.
    // The full 3000 x's should not appear.
    assert!(
        !md.contains(&"x".repeat(3000)),
        "full 3000-byte stderr should not appear in output"
    );
    // But a truncated version should be present.
    assert!(
        md.contains("..."),
        "truncated stderr should end with ..."
    );

    pool.close().await;
    drop_temp_db(&db_name).await;
}
