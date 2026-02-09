//! Integration tests for plan and task CRUD operations.
//!
//! These tests require a running PostgreSQL instance accessible via
//! `GATOR_DATABASE_URL` (or the default `postgresql://localhost:5432/gator`).
//!
//! Each test creates a unique temporary database, runs migrations, and drops
//! it on completion so tests are fully isolated.

use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgPool};
use uuid::Uuid;

use gator_db::config::DbConfig;
use gator_db::models::{PlanStatus, TaskStatus};
use gator_db::pool;
use gator_db::queries::{plans, tasks};

/// Helper: create a unique temporary database and return a pool pointing at it.
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
        .max_connections(2)
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

// -----------------------------------------------------------------------
// Plan CRUD tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn insert_and_get_plan() {
    let (pool, db_name) = create_temp_db().await;

    let plan = plans::insert_plan(
        &pool,
        "test-plan",
        "/tmp/project",
        "main",
        None,
        "claude-code",
        "worktree",
    )
    .await
    .expect("insert_plan should succeed");

    assert_eq!(plan.name, "test-plan");
    assert_eq!(plan.project_path, "/tmp/project");
    assert_eq!(plan.base_branch, "main");
    assert_eq!(plan.status, PlanStatus::Draft);
    assert!(plan.approved_at.is_none());
    assert!(plan.completed_at.is_none());

    // Fetch it back.
    let fetched = plans::get_plan(&pool, plan.id)
        .await
        .expect("get_plan should succeed")
        .expect("plan should exist");

    assert_eq!(fetched.id, plan.id);
    assert_eq!(fetched.name, "test-plan");

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn get_plan_returns_none_for_missing_id() {
    let (pool, db_name) = create_temp_db().await;

    let result = plans::get_plan(&pool, Uuid::new_v4())
        .await
        .expect("get_plan should not error");

    assert!(result.is_none());

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn list_plans_returns_all() {
    let (pool, db_name) = create_temp_db().await;

    plans::insert_plan(
        &pool,
        "plan-a",
        "/tmp/a",
        "main",
        None,
        "claude-code",
        "worktree",
    )
    .await
    .unwrap();
    plans::insert_plan(
        &pool,
        "plan-b",
        "/tmp/b",
        "develop",
        None,
        "claude-code",
        "worktree",
    )
    .await
    .unwrap();

    let all = plans::list_plans(&pool).await.unwrap();
    assert_eq!(all.len(), 2);

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn update_plan_status_succeeds() {
    let (pool, db_name) = create_temp_db().await;

    let plan = plans::insert_plan(
        &pool,
        "status-test",
        "/tmp",
        "main",
        None,
        "claude-code",
        "worktree",
    )
    .await
    .unwrap();

    assert_eq!(plan.status, PlanStatus::Draft);

    plans::update_plan_status(&pool, plan.id, PlanStatus::Approved)
        .await
        .expect("update should succeed");

    let updated = plans::get_plan(&pool, plan.id).await.unwrap().unwrap();
    assert_eq!(updated.status, PlanStatus::Approved);

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn update_plan_status_fails_for_missing_plan() {
    let (pool, db_name) = create_temp_db().await;

    let result = plans::update_plan_status(&pool, Uuid::new_v4(), PlanStatus::Approved).await;
    assert!(result.is_err());

    pool.close().await;
    drop_temp_db(&db_name).await;
}

// -----------------------------------------------------------------------
// Task CRUD tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn insert_and_get_task() {
    let (pool, db_name) = create_temp_db().await;

    let plan = plans::insert_plan(
        &pool,
        "task-test-plan",
        "/tmp",
        "main",
        None,
        "claude-code",
        "worktree",
    )
    .await
    .unwrap();

    let task = tasks::insert_task(
        &pool,
        plan.id,
        "task-one",
        "Do the first thing",
        "narrow",
        "auto",
        3,
        None,
    )
    .await
    .expect("insert_task should succeed");

    assert_eq!(task.plan_id, plan.id);
    assert_eq!(task.name, "task-one");
    assert_eq!(task.description, "Do the first thing");
    assert_eq!(task.status, TaskStatus::Pending);
    assert_eq!(task.attempt, 0);
    assert_eq!(task.retry_max, 3);

    // Fetch it back.
    let fetched = tasks::get_task(&pool, task.id)
        .await
        .expect("get_task should succeed")
        .expect("task should exist");

    assert_eq!(fetched.id, task.id);
    assert_eq!(fetched.name, "task-one");

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn list_tasks_for_plan_returns_correct_tasks() {
    let (pool, db_name) = create_temp_db().await;

    let plan_a = plans::insert_plan(
        &pool,
        "plan-a",
        "/tmp/a",
        "main",
        None,
        "claude-code",
        "worktree",
    )
    .await
    .unwrap();
    let plan_b = plans::insert_plan(
        &pool,
        "plan-b",
        "/tmp/b",
        "main",
        None,
        "claude-code",
        "worktree",
    )
    .await
    .unwrap();

    tasks::insert_task(
        &pool, plan_a.id, "a-task-1", "desc", "narrow", "auto", 3, None,
    )
    .await
    .unwrap();
    tasks::insert_task(
        &pool,
        plan_a.id,
        "a-task-2",
        "desc",
        "medium",
        "human_review",
        2,
        None,
    )
    .await
    .unwrap();
    tasks::insert_task(
        &pool,
        plan_b.id,
        "b-task-1",
        "desc",
        "broad",
        "human_approve",
        1,
        None,
    )
    .await
    .unwrap();

    let plan_a_tasks = tasks::list_tasks_for_plan(&pool, plan_a.id).await.unwrap();
    assert_eq!(plan_a_tasks.len(), 2);

    let plan_b_tasks = tasks::list_tasks_for_plan(&pool, plan_b.id).await.unwrap();
    assert_eq!(plan_b_tasks.len(), 1);

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn update_task_status_succeeds() {
    let (pool, db_name) = create_temp_db().await;

    let plan = plans::insert_plan(&pool, "p", "/tmp", "main", None, "claude-code", "worktree")
        .await
        .unwrap();
    let task = tasks::insert_task(&pool, plan.id, "t", "d", "narrow", "auto", 3, None)
        .await
        .unwrap();

    tasks::update_task_status(&pool, task.id, TaskStatus::Assigned)
        .await
        .expect("update should succeed");

    let updated = tasks::get_task(&pool, task.id).await.unwrap().unwrap();
    assert_eq!(updated.status, TaskStatus::Assigned);

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn task_dependencies_roundtrip() {
    let (pool, db_name) = create_temp_db().await;

    let plan = plans::insert_plan(
        &pool,
        "dep-test",
        "/tmp",
        "main",
        None,
        "claude-code",
        "worktree",
    )
    .await
    .unwrap();

    let task_a = tasks::insert_task(&pool, plan.id, "a", "first", "narrow", "auto", 3, None)
        .await
        .unwrap();
    let task_b = tasks::insert_task(&pool, plan.id, "b", "second", "narrow", "auto", 3, None)
        .await
        .unwrap();
    let task_c = tasks::insert_task(&pool, plan.id, "c", "third", "narrow", "auto", 3, None)
        .await
        .unwrap();

    // b depends on a; c depends on a and b.
    tasks::insert_task_dependency(&pool, task_b.id, task_a.id)
        .await
        .unwrap();
    tasks::insert_task_dependency(&pool, task_c.id, task_a.id)
        .await
        .unwrap();
    tasks::insert_task_dependency(&pool, task_c.id, task_b.id)
        .await
        .unwrap();

    let b_deps = tasks::get_task_dependencies(&pool, task_b.id)
        .await
        .unwrap();
    assert_eq!(b_deps, vec![task_a.id]);

    let mut c_deps = tasks::get_task_dependencies(&pool, task_c.id)
        .await
        .unwrap();
    c_deps.sort();
    let mut expected = vec![task_a.id, task_b.id];
    expected.sort();
    assert_eq!(c_deps, expected);

    let a_deps = tasks::get_task_dependencies(&pool, task_a.id)
        .await
        .unwrap();
    assert!(a_deps.is_empty());

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn task_dependency_is_idempotent() {
    let (pool, db_name) = create_temp_db().await;

    let plan = plans::insert_plan(
        &pool,
        "idem",
        "/tmp",
        "main",
        None,
        "claude-code",
        "worktree",
    )
    .await
    .unwrap();
    let a = tasks::insert_task(&pool, plan.id, "a", "d", "narrow", "auto", 3, None)
        .await
        .unwrap();
    let b = tasks::insert_task(&pool, plan.id, "b", "d", "narrow", "auto", 3, None)
        .await
        .unwrap();

    // Insert same dependency twice -- should not error.
    tasks::insert_task_dependency(&pool, b.id, a.id)
        .await
        .unwrap();
    tasks::insert_task_dependency(&pool, b.id, a.id)
        .await
        .unwrap();

    let deps = tasks::get_task_dependencies(&pool, b.id).await.unwrap();
    assert_eq!(deps.len(), 1);

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn link_task_invariant_roundtrip() {
    let (pool, db_name) = create_temp_db().await;

    let plan = plans::insert_plan(
        &pool,
        "inv-link",
        "/tmp",
        "main",
        None,
        "claude-code",
        "worktree",
    )
    .await
    .unwrap();
    let task = tasks::insert_task(&pool, plan.id, "t", "d", "narrow", "auto", 3, None)
        .await
        .unwrap();

    // Insert an invariant directly for testing.
    let inv_row: (Uuid,) = sqlx::query_as(
        "INSERT INTO invariants (name, kind, command) VALUES ('test_inv', 'custom', 'true') \
         RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    tasks::link_task_invariant(&pool, task.id, inv_row.0)
        .await
        .unwrap();

    // Verify link exists.
    let linked: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM task_invariants WHERE task_id = $1 AND invariant_id = $2",
    )
    .bind(task.id)
    .bind(inv_row.0)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(linked.0, 1);

    // Idempotent: linking again should not error or duplicate.
    tasks::link_task_invariant(&pool, task.id, inv_row.0)
        .await
        .unwrap();

    let linked2: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM task_invariants WHERE task_id = $1 AND invariant_id = $2",
    )
    .bind(task.id)
    .bind(inv_row.0)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(linked2.0, 1);

    pool.close().await;
    drop_temp_db(&db_name).await;
}

// -----------------------------------------------------------------------
// Plan timestamp tests (T023)
// -----------------------------------------------------------------------

#[tokio::test]
async fn update_plan_status_to_completed_sets_completed_at() {
    let (pool, db_name) = create_temp_db().await;

    let plan = plans::insert_plan(
        &pool,
        "ts-completed",
        "/tmp",
        "main",
        None,
        "claude-code",
        "worktree",
    )
    .await
    .unwrap();

    assert!(plan.completed_at.is_none());

    plans::update_plan_status(&pool, plan.id, PlanStatus::Completed)
        .await
        .unwrap();

    let updated = plans::get_plan(&pool, plan.id).await.unwrap().unwrap();
    assert_eq!(updated.status, PlanStatus::Completed);
    assert!(
        updated.completed_at.is_some(),
        "completed_at should be set when transitioning to completed"
    );

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn update_plan_status_to_failed_sets_completed_at() {
    let (pool, db_name) = create_temp_db().await;

    let plan = plans::insert_plan(
        &pool,
        "ts-failed",
        "/tmp",
        "main",
        None,
        "claude-code",
        "worktree",
    )
    .await
    .unwrap();

    plans::update_plan_status(&pool, plan.id, PlanStatus::Failed)
        .await
        .unwrap();

    let updated = plans::get_plan(&pool, plan.id).await.unwrap().unwrap();
    assert_eq!(updated.status, PlanStatus::Failed);
    assert!(
        updated.completed_at.is_some(),
        "completed_at should be set when transitioning to failed"
    );

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn update_plan_status_to_approved_sets_approved_at() {
    let (pool, db_name) = create_temp_db().await;

    let plan = plans::insert_plan(
        &pool,
        "ts-approved",
        "/tmp",
        "main",
        None,
        "claude-code",
        "worktree",
    )
    .await
    .unwrap();

    assert!(plan.approved_at.is_none());

    plans::update_plan_status(&pool, plan.id, PlanStatus::Approved)
        .await
        .unwrap();

    let updated = plans::get_plan(&pool, plan.id).await.unwrap().unwrap();
    assert_eq!(updated.status, PlanStatus::Approved);
    assert!(
        updated.approved_at.is_some(),
        "approved_at should be set when transitioning to approved"
    );

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn update_plan_status_does_not_overwrite_existing_timestamps() {
    let (pool, db_name) = create_temp_db().await;

    // Use approve_plan to set approved_at via the explicit path.
    let plan = plans::insert_plan(
        &pool,
        "ts-no-overwrite",
        "/tmp",
        "main",
        None,
        "claude-code",
        "worktree",
    )
    .await
    .unwrap();
    let approved = plans::approve_plan(&pool, plan.id).await.unwrap();
    let original_approved_at = approved.approved_at.unwrap();

    // Transition to running then back to approved via update_plan_status.
    plans::update_plan_status(&pool, plan.id, PlanStatus::Running)
        .await
        .unwrap();
    plans::update_plan_status(&pool, plan.id, PlanStatus::Approved)
        .await
        .unwrap();

    let updated = plans::get_plan(&pool, plan.id).await.unwrap().unwrap();
    assert_eq!(
        updated.approved_at.unwrap(),
        original_approved_at,
        "approved_at should not be overwritten"
    );

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn update_plan_status_to_running_does_not_set_timestamps() {
    let (pool, db_name) = create_temp_db().await;

    let plan = plans::insert_plan(
        &pool,
        "ts-running",
        "/tmp",
        "main",
        None,
        "claude-code",
        "worktree",
    )
    .await
    .unwrap();

    plans::update_plan_status(&pool, plan.id, PlanStatus::Running)
        .await
        .unwrap();

    let updated = plans::get_plan(&pool, plan.id).await.unwrap().unwrap();
    assert_eq!(updated.status, PlanStatus::Running);
    assert!(
        updated.approved_at.is_none(),
        "approved_at should not be set for running"
    );
    assert!(
        updated.completed_at.is_none(),
        "completed_at should not be set for running"
    );

    pool.close().await;
    drop_temp_db(&db_name).await;
}
