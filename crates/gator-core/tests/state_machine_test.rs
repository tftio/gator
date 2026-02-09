//! Integration tests for the task state machine (T014).
//!
//! These tests require a running PostgreSQL instance accessible via
//! `GATOR_DATABASE_URL` (or the default `postgresql://localhost:5432/gator`).
//!
//! Each test creates a unique temporary database, runs migrations, and drops
//! it on completion so tests are fully isolated and idempotent.

use std::path::Path;
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgPool};
use uuid::Uuid;

use gator_db::config::DbConfig;
use gator_db::models::TaskStatus;
use gator_db::pool;
use gator_db::queries::tasks as db;

use gator_core::state::TaskStateMachine;
use gator_core::state::dispatch;
use gator_core::state::queries;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Create a unique temporary database and return a pool pointing at it.
async fn create_temp_db() -> (PgPool, String) {
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

    let temp_url = match base_config.database_url.rfind('/') {
        Some(pos) => format!("{}/{db_name}", &base_config.database_url[..pos]),
        None => panic!("cannot parse database URL"),
    };

    let temp_pool = PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&temp_url)
        .await
        .unwrap_or_else(|e| panic!("failed to connect to temp database {db_name}: {e}"));

    // Run migrations.
    let migrations_path = pool::default_migrations_path();
    pool::run_migrations(&temp_pool, migrations_path)
        .await
        .expect("migrations should succeed");

    (temp_pool, db_name)
}

/// Drop the temporary database.
async fn drop_temp_db(db_name: &str) {
    let base_config = DbConfig::from_env();
    let maint_url = base_config.maintenance_url();

    let maint_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&maint_url)
        .await
        .expect("failed to connect to maintenance database for cleanup");

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

/// Insert a plan and return its UUID.
async fn create_test_plan(pool: &PgPool) -> Uuid {
    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO plans (name, project_path, base_branch) \
         VALUES ('test-plan', '/tmp/project', 'main') \
         RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("failed to insert test plan");
    row.0
}

/// Insert a task for a plan and return it.
async fn create_test_task(
    pool: &PgPool,
    plan_id: Uuid,
    name: &str,
    retry_max: i32,
) -> gator_db::models::Task {
    db::insert_task(
        pool,
        plan_id,
        name,
        "test description",
        "narrow",
        "auto",
        retry_max,
        None,
    )
    .await
    .expect("failed to insert test task")
}

// ---------------------------------------------------------------------------
// Unit tests: transition validation (no DB needed)
// ---------------------------------------------------------------------------

#[test]
fn valid_transitions_accepted() {
    let valid = [
        (TaskStatus::Pending, TaskStatus::Assigned),
        (TaskStatus::Assigned, TaskStatus::Running),
        (TaskStatus::Running, TaskStatus::Checking),
        (TaskStatus::Checking, TaskStatus::Passed),
        (TaskStatus::Checking, TaskStatus::Failed),
        (TaskStatus::Failed, TaskStatus::Assigned),
        (TaskStatus::Failed, TaskStatus::Escalated),
    ];
    for (from, to) in &valid {
        assert!(
            TaskStateMachine::is_valid_transition(*from, *to),
            "expected {from} -> {to} to be valid"
        );
    }
}

#[test]
fn invalid_transitions_rejected() {
    let invalid = [
        (TaskStatus::Pending, TaskStatus::Running),
        (TaskStatus::Pending, TaskStatus::Checking),
        (TaskStatus::Pending, TaskStatus::Passed),
        (TaskStatus::Pending, TaskStatus::Failed),
        (TaskStatus::Pending, TaskStatus::Escalated),
        (TaskStatus::Assigned, TaskStatus::Pending),
        (TaskStatus::Assigned, TaskStatus::Checking),
        (TaskStatus::Assigned, TaskStatus::Passed),
        (TaskStatus::Assigned, TaskStatus::Failed),
        (TaskStatus::Running, TaskStatus::Pending),
        (TaskStatus::Running, TaskStatus::Assigned),
        (TaskStatus::Running, TaskStatus::Passed),
        (TaskStatus::Running, TaskStatus::Failed),
        (TaskStatus::Checking, TaskStatus::Pending),
        (TaskStatus::Checking, TaskStatus::Assigned),
        (TaskStatus::Checking, TaskStatus::Running),
        (TaskStatus::Passed, TaskStatus::Pending),
        (TaskStatus::Passed, TaskStatus::Failed),
        (TaskStatus::Failed, TaskStatus::Running),
        (TaskStatus::Failed, TaskStatus::Checking),
        (TaskStatus::Failed, TaskStatus::Passed),
        (TaskStatus::Escalated, TaskStatus::Assigned),
    ];
    for (from, to) in &invalid {
        assert!(
            !TaskStateMachine::is_valid_transition(*from, *to),
            "expected {from} -> {to} to be invalid"
        );
    }
}

// ---------------------------------------------------------------------------
// Integration tests: state transitions against a real database
// ---------------------------------------------------------------------------

#[tokio::test]
async fn happy_path_full_lifecycle() {
    let (pool, db_name) = create_temp_db().await;

    let plan_id = create_test_plan(&pool).await;
    let task = create_test_task(&pool, plan_id, "lifecycle-task", 3).await;

    // pending -> assigned
    dispatch::assign_task(&pool, task.id, "test-harness", Path::new("/tmp/wt"))
        .await
        .expect("assign should succeed");

    let t = db::get_task(&pool, task.id).await.unwrap().unwrap();
    assert_eq!(t.status, TaskStatus::Assigned);
    assert_eq!(t.assigned_harness.as_deref(), Some("test-harness"));
    assert_eq!(t.worktree_path.as_deref(), Some("/tmp/wt"));

    // assigned -> running
    dispatch::start_task(&pool, task.id)
        .await
        .expect("start should succeed");

    let t = db::get_task(&pool, task.id).await.unwrap().unwrap();
    assert_eq!(t.status, TaskStatus::Running);
    assert!(t.started_at.is_some(), "started_at should be set");

    // running -> checking
    dispatch::begin_checking(&pool, task.id)
        .await
        .expect("begin_checking should succeed");

    let t = db::get_task(&pool, task.id).await.unwrap().unwrap();
    assert_eq!(t.status, TaskStatus::Checking);

    // checking -> passed
    dispatch::pass_task(&pool, task.id)
        .await
        .expect("pass should succeed");

    let t = db::get_task(&pool, task.id).await.unwrap().unwrap();
    assert_eq!(t.status, TaskStatus::Passed);
    assert!(t.completed_at.is_some(), "completed_at should be set");

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn failure_and_retry_lifecycle() {
    let (pool, db_name) = create_temp_db().await;

    let plan_id = create_test_plan(&pool).await;
    let task = create_test_task(&pool, plan_id, "retry-task", 3).await;

    // Move to checking
    dispatch::assign_task(&pool, task.id, "h", Path::new("/tmp/wt"))
        .await
        .unwrap();
    dispatch::start_task(&pool, task.id).await.unwrap();
    dispatch::begin_checking(&pool, task.id).await.unwrap();

    // checking -> failed
    dispatch::fail_task(&pool, task.id).await.unwrap();
    let t = db::get_task(&pool, task.id).await.unwrap().unwrap();
    assert_eq!(t.status, TaskStatus::Failed);
    assert!(t.completed_at.is_some());

    // failed -> assigned (retry)
    dispatch::retry_task(&pool, task.id).await.unwrap();
    let t = db::get_task(&pool, task.id).await.unwrap().unwrap();
    assert_eq!(t.status, TaskStatus::Assigned);
    assert_eq!(t.attempt, 1, "attempt should be incremented");
    assert!(
        t.started_at.is_none(),
        "started_at should be cleared on retry"
    );
    assert!(
        t.completed_at.is_none(),
        "completed_at should be cleared on retry"
    );

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn escalation_lifecycle() {
    let (pool, db_name) = create_temp_db().await;

    let plan_id = create_test_plan(&pool).await;
    let task = create_test_task(&pool, plan_id, "escalate-task", 3).await;

    // Move to failed
    dispatch::assign_task(&pool, task.id, "h", Path::new("/tmp/wt"))
        .await
        .unwrap();
    dispatch::start_task(&pool, task.id).await.unwrap();
    dispatch::begin_checking(&pool, task.id).await.unwrap();
    dispatch::fail_task(&pool, task.id).await.unwrap();

    // failed -> escalated
    dispatch::escalate_task(&pool, task.id).await.unwrap();
    let t = db::get_task(&pool, task.id).await.unwrap().unwrap();
    assert_eq!(t.status, TaskStatus::Escalated);
    assert!(t.completed_at.is_some());

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn invalid_transition_rejected_at_db_level() {
    let (pool, db_name) = create_temp_db().await;

    let plan_id = create_test_plan(&pool).await;
    let task = create_test_task(&pool, plan_id, "invalid-trans", 3).await;

    // Try to go pending -> running (skipping assigned)
    let result =
        TaskStateMachine::transition(&pool, task.id, TaskStatus::Pending, TaskStatus::Running)
            .await;
    assert!(result.is_err(), "pending -> running should fail");
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("invalid state transition"),
        "error should mention invalid transition: {err_msg}"
    );

    // Verify status unchanged
    let t = db::get_task(&pool, task.id).await.unwrap().unwrap();
    assert_eq!(t.status, TaskStatus::Pending);

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn optimistic_lock_prevents_double_transition() {
    let (pool, db_name) = create_temp_db().await;

    let plan_id = create_test_plan(&pool).await;
    let task = create_test_task(&pool, plan_id, "double-trans", 3).await;

    // Assign the task
    dispatch::assign_task(&pool, task.id, "h", Path::new("/tmp/wt"))
        .await
        .unwrap();

    // Start the task (assigned -> running)
    dispatch::start_task(&pool, task.id).await.unwrap();

    // Try to start it again (should fail because it is now running, not assigned)
    let result = dispatch::start_task(&pool, task.id).await;
    assert!(result.is_err(), "double start should fail");
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("optimistic lock failed"),
        "error should mention optimistic lock: {err_msg}"
    );

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn retry_respects_retry_max() {
    let (pool, db_name) = create_temp_db().await;

    let plan_id = create_test_plan(&pool).await;
    // retry_max = 2 means attempts 0 and 1 are allowed; attempt 2 should fail
    let task = create_test_task(&pool, plan_id, "retry-max-task", 2).await;

    // First pass: attempt 0 -> fail
    dispatch::assign_task(&pool, task.id, "h", Path::new("/tmp/wt"))
        .await
        .unwrap();
    dispatch::start_task(&pool, task.id).await.unwrap();
    dispatch::begin_checking(&pool, task.id).await.unwrap();
    dispatch::fail_task(&pool, task.id).await.unwrap();

    // Retry: attempt 0 -> 1
    dispatch::retry_task(&pool, task.id).await.unwrap();
    let t = db::get_task(&pool, task.id).await.unwrap().unwrap();
    assert_eq!(t.attempt, 1);

    // Second pass: attempt 1 -> fail
    dispatch::start_task(&pool, task.id).await.unwrap();
    dispatch::begin_checking(&pool, task.id).await.unwrap();
    dispatch::fail_task(&pool, task.id).await.unwrap();

    // Retry: attempt 1 -> 2 should fail (1 >= retry_max 2? No, 1 < 2 so ok)
    dispatch::retry_task(&pool, task.id).await.unwrap();
    let t = db::get_task(&pool, task.id).await.unwrap().unwrap();
    assert_eq!(t.attempt, 2);

    // Third pass: attempt 2 -> fail
    dispatch::start_task(&pool, task.id).await.unwrap();
    dispatch::begin_checking(&pool, task.id).await.unwrap();
    dispatch::fail_task(&pool, task.id).await.unwrap();

    // Retry: attempt 2 -> 3 should fail (2 >= retry_max 2)
    let result = dispatch::retry_task(&pool, task.id).await;
    assert!(result.is_err(), "retry beyond retry_max should fail");
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("retry_max"),
        "error should mention retry_max: {err_msg}"
    );

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn timestamps_set_correctly() {
    let (pool, db_name) = create_temp_db().await;

    let plan_id = create_test_plan(&pool).await;
    let task = create_test_task(&pool, plan_id, "timestamp-task", 3).await;

    // Initially no timestamps
    assert!(task.started_at.is_none());
    assert!(task.completed_at.is_none());

    // Assign
    dispatch::assign_task(&pool, task.id, "h", Path::new("/tmp/wt"))
        .await
        .unwrap();
    let t = db::get_task(&pool, task.id).await.unwrap().unwrap();
    assert!(
        t.started_at.is_none(),
        "started_at should still be None after assign"
    );

    // Start: should set started_at
    let before_start = chrono::Utc::now();
    dispatch::start_task(&pool, task.id).await.unwrap();
    let t = db::get_task(&pool, task.id).await.unwrap().unwrap();
    assert!(
        t.started_at.is_some(),
        "started_at should be set after start"
    );
    assert!(
        t.started_at.unwrap() >= before_start,
        "started_at should be >= the time before start"
    );

    // Check
    dispatch::begin_checking(&pool, task.id).await.unwrap();

    // Pass: should set completed_at
    let before_pass = chrono::Utc::now();
    dispatch::pass_task(&pool, task.id).await.unwrap();
    let t = db::get_task(&pool, task.id).await.unwrap().unwrap();
    assert!(
        t.completed_at.is_some(),
        "completed_at should be set after pass"
    );
    assert!(
        t.completed_at.unwrap() >= before_pass,
        "completed_at should be >= the time before pass"
    );

    pool.close().await;
    drop_temp_db(&db_name).await;
}

// ---------------------------------------------------------------------------
// Dependency checks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dependency_check_blocks_assignment() {
    let (pool, db_name) = create_temp_db().await;

    let plan_id = create_test_plan(&pool).await;
    let dep_task = create_test_task(&pool, plan_id, "dep-task", 3).await;
    let main_task = create_test_task(&pool, plan_id, "main-task", 3).await;

    // main depends on dep
    db::insert_task_dependency(&pool, main_task.id, dep_task.id)
        .await
        .unwrap();

    // Try to assign main while dep is still pending
    let result = dispatch::assign_task(&pool, main_task.id, "h", Path::new("/tmp/wt")).await;
    assert!(result.is_err(), "assign should fail when dep is pending");
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("dep-task"),
        "error should mention the dependency name: {err_msg}"
    );

    // Move dep all the way to passed
    dispatch::assign_task(&pool, dep_task.id, "h", Path::new("/tmp/wt"))
        .await
        .unwrap();
    dispatch::start_task(&pool, dep_task.id).await.unwrap();
    dispatch::begin_checking(&pool, dep_task.id).await.unwrap();
    dispatch::pass_task(&pool, dep_task.id).await.unwrap();

    // Now main should be assignable
    dispatch::assign_task(&pool, main_task.id, "h", Path::new("/tmp/wt"))
        .await
        .expect("assign should succeed after dep is passed");

    let t = db::get_task(&pool, main_task.id).await.unwrap().unwrap();
    assert_eq!(t.status, TaskStatus::Assigned);

    pool.close().await;
    drop_temp_db(&db_name).await;
}

// ---------------------------------------------------------------------------
// Query helpers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_ready_tasks_returns_correct_results() {
    let (pool, db_name) = create_temp_db().await;

    let plan_id = create_test_plan(&pool).await;
    let task_a = create_test_task(&pool, plan_id, "task-a", 3).await;
    let task_b = create_test_task(&pool, plan_id, "task-b", 3).await;
    let task_c = create_test_task(&pool, plan_id, "task-c", 3).await;

    // B depends on A, C has no dependencies
    db::insert_task_dependency(&pool, task_b.id, task_a.id)
        .await
        .unwrap();

    // Initially: A and C should be ready (no unfulfilled deps), B should not
    let ready = queries::get_ready_tasks(&pool, plan_id).await.unwrap();
    let ready_ids: Vec<Uuid> = ready.iter().map(|t| t.id).collect();
    assert!(ready_ids.contains(&task_a.id), "A should be ready");
    assert!(ready_ids.contains(&task_c.id), "C should be ready");
    assert!(
        !ready_ids.contains(&task_b.id),
        "B should not be ready (dep A pending)"
    );

    // Pass A
    dispatch::assign_task(&pool, task_a.id, "h", Path::new("/tmp/wt"))
        .await
        .unwrap();
    dispatch::start_task(&pool, task_a.id).await.unwrap();
    dispatch::begin_checking(&pool, task_a.id).await.unwrap();
    dispatch::pass_task(&pool, task_a.id).await.unwrap();

    // Now B should be ready
    let ready = queries::get_ready_tasks(&pool, plan_id).await.unwrap();
    let ready_ids: Vec<Uuid> = ready.iter().map(|t| t.id).collect();
    assert!(
        ready_ids.contains(&task_b.id),
        "B should be ready after A passed"
    );
    assert!(ready_ids.contains(&task_c.id), "C should still be ready");
    assert!(
        !ready_ids.contains(&task_a.id),
        "A should not be ready (status=passed, not pending)"
    );

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn plan_progress_and_completion() {
    let (pool, db_name) = create_temp_db().await;

    let plan_id = create_test_plan(&pool).await;
    let task_a = create_test_task(&pool, plan_id, "prog-a", 3).await;
    let task_b = create_test_task(&pool, plan_id, "prog-b", 3).await;

    // Initially: 2 pending
    let progress = queries::get_plan_progress(&pool, plan_id).await.unwrap();
    assert_eq!(progress.pending, 2);
    assert_eq!(progress.total, 2);
    assert!(!queries::is_plan_complete(&pool, plan_id).await.unwrap());

    // Pass task A
    dispatch::assign_task(&pool, task_a.id, "h", Path::new("/tmp/wt"))
        .await
        .unwrap();
    dispatch::start_task(&pool, task_a.id).await.unwrap();
    dispatch::begin_checking(&pool, task_a.id).await.unwrap();
    dispatch::pass_task(&pool, task_a.id).await.unwrap();

    let progress = queries::get_plan_progress(&pool, plan_id).await.unwrap();
    assert_eq!(progress.pending, 1);
    assert_eq!(progress.passed, 1);
    assert!(!queries::is_plan_complete(&pool, plan_id).await.unwrap());

    // Pass task B
    dispatch::assign_task(&pool, task_b.id, "h", Path::new("/tmp/wt"))
        .await
        .unwrap();
    dispatch::start_task(&pool, task_b.id).await.unwrap();
    dispatch::begin_checking(&pool, task_b.id).await.unwrap();
    dispatch::pass_task(&pool, task_b.id).await.unwrap();

    let progress = queries::get_plan_progress(&pool, plan_id).await.unwrap();
    assert_eq!(progress.passed, 2);
    assert_eq!(progress.pending, 0);
    assert!(queries::is_plan_complete(&pool, plan_id).await.unwrap());

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn concurrent_transitions_handled_safely() {
    let (pool, db_name) = create_temp_db().await;

    let plan_id = create_test_plan(&pool).await;
    let task = create_test_task(&pool, plan_id, "concurrent-task", 3).await;

    // Assign the task
    dispatch::assign_task(&pool, task.id, "h", Path::new("/tmp/wt"))
        .await
        .unwrap();

    // Launch two concurrent start_task calls
    let pool2 = pool.clone();
    let task_id = task.id;
    let handle1 = tokio::spawn(async move { dispatch::start_task(&pool2, task_id).await });
    let pool3 = pool.clone();
    let handle2 = tokio::spawn(async move { dispatch::start_task(&pool3, task_id).await });

    let result1 = handle1.await.unwrap();
    let result2 = handle2.await.unwrap();

    // Exactly one should succeed, one should fail
    let successes = [result1.is_ok(), result2.is_ok()]
        .iter()
        .filter(|x| **x)
        .count();
    assert_eq!(
        successes, 1,
        "exactly one concurrent transition should succeed, but {successes} did"
    );

    // Final state should be running
    let t = db::get_task(&pool, task.id).await.unwrap().unwrap();
    assert_eq!(t.status, TaskStatus::Running);

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn task_not_found_gives_clear_error() {
    let (pool, db_name) = create_temp_db().await;

    let fake_id = Uuid::new_v4();
    let result =
        TaskStateMachine::transition(&pool, fake_id, TaskStatus::Pending, TaskStatus::Assigned)
            .await;

    assert!(result.is_err());
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("not found"),
        "error should say 'not found': {err_msg}"
    );

    pool.close().await;
    drop_temp_db(&db_name).await;
}
