//! Integration tests for the gate runner (T015).
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
use gator_db::queries::gate_results;
use gator_db::queries::invariants::{self, NewInvariant};
use gator_db::queries::tasks as task_db;

use gator_core::gate::evaluator::{evaluate_verdict, GateAction};
use gator_core::gate::{GateRunner, GateVerdict};
use gator_core::state::dispatch;

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

/// Insert a task with the given gate policy.
async fn create_test_task(
    pool: &PgPool,
    plan_id: Uuid,
    name: &str,
    gate_policy: &str,
    retry_max: i32,
) -> gator_db::models::Task {
    task_db::insert_task(
        pool,
        plan_id,
        name,
        "test task description",
        "narrow",
        gate_policy,
        retry_max,
        None,
    )
    .await
    .expect("failed to insert test task")
}

/// Insert an invariant that runs the given command.
async fn create_test_invariant(
    pool: &PgPool,
    name: &str,
    command: &str,
    args: &[String],
    expected_exit_code: i32,
) -> gator_db::models::Invariant {
    let new = NewInvariant {
        name,
        description: Some("test invariant"),
        kind: gator_db::models::InvariantKind::Custom,
        command,
        args,
        expected_exit_code,
        threshold: None,
        scope: gator_db::models::InvariantScope::Project,
    };
    invariants::insert_invariant(pool, &new)
        .await
        .expect("failed to insert test invariant")
}

/// Move a task from pending through to running state, setting up
/// worktree metadata.
async fn advance_task_to_running(pool: &PgPool, task_id: Uuid, worktree_path: &str) {
    dispatch::assign_task(pool, task_id, "test-harness", Path::new(worktree_path))
        .await
        .expect("assign should succeed");
    dispatch::start_task(pool, task_id)
        .await
        .expect("start should succeed");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn all_invariants_pass_auto_gate_passes_task() {
    let (pool, db_name) = create_temp_db().await;

    let plan_id = create_test_plan(&pool).await;
    let task = create_test_task(&pool, plan_id, "pass-task", "auto", 3).await;

    // Create two invariants that always pass.
    let inv1 = create_test_invariant(&pool, "always_true_1", "true", &[], 0).await;
    let inv2 = create_test_invariant(&pool, "always_true_2", "true", &[], 0).await;

    // Link invariants to task.
    invariants::link_task_invariant(&pool, task.id, inv1.id)
        .await
        .unwrap();
    invariants::link_task_invariant(&pool, task.id, inv2.id)
        .await
        .unwrap();

    // Advance task to running (worktree set to /tmp).
    advance_task_to_running(&pool, task.id, "/tmp").await;

    // Run the gate.
    let runner = GateRunner::new(&pool);
    let verdict = runner.run_gate(task.id).await.expect("run_gate should succeed");

    // Verdict should be Passed.
    assert!(
        matches!(verdict, GateVerdict::Passed),
        "expected GateVerdict::Passed, got {:?}",
        verdict
    );

    // Task should be in checking state after run_gate.
    let t = task_db::get_task(&pool, task.id).await.unwrap().unwrap();
    assert_eq!(t.status, TaskStatus::Checking);

    // Evaluate the verdict (auto policy should pass the task).
    let action = evaluate_verdict(&pool, task.id, &verdict)
        .await
        .expect("evaluate should succeed");
    assert_eq!(action, GateAction::AutoPassed);

    // Task should now be passed.
    let t = task_db::get_task(&pool, task.id).await.unwrap().unwrap();
    assert_eq!(t.status, TaskStatus::Passed);
    assert!(t.completed_at.is_some());

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn one_invariant_fails_auto_gate_fails_task() {
    let (pool, db_name) = create_temp_db().await;

    let plan_id = create_test_plan(&pool).await;
    let task = create_test_task(&pool, plan_id, "fail-task", "auto", 3).await;

    // One passing invariant, one failing.
    let inv_pass = create_test_invariant(&pool, "pass_inv", "true", &[], 0).await;
    let inv_fail = create_test_invariant(&pool, "fail_inv", "false", &[], 0).await;

    invariants::link_task_invariant(&pool, task.id, inv_pass.id)
        .await
        .unwrap();
    invariants::link_task_invariant(&pool, task.id, inv_fail.id)
        .await
        .unwrap();

    advance_task_to_running(&pool, task.id, "/tmp").await;

    let runner = GateRunner::new(&pool);
    let verdict = runner.run_gate(task.id).await.expect("run_gate should succeed");

    // Verdict should be Failed with one failure.
    match &verdict {
        GateVerdict::Failed { failures } => {
            assert_eq!(failures.len(), 1, "should have exactly one failure");
            assert_eq!(failures[0].invariant_name, "fail_inv");
            assert_eq!(failures[0].exit_code, Some(1));
        }
        GateVerdict::Passed => panic!("expected Failed verdict, got Passed"),
    }

    // Evaluate: should auto-fail with retry eligibility.
    let action = evaluate_verdict(&pool, task.id, &verdict)
        .await
        .expect("evaluate should succeed");
    assert_eq!(
        action,
        GateAction::AutoFailed { can_retry: true },
    );

    // Task should be in failed state.
    let t = task_db::get_task(&pool, task.id).await.unwrap().unwrap();
    assert_eq!(t.status, TaskStatus::Failed);

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn human_review_gate_leaves_task_in_checking() {
    let (pool, db_name) = create_temp_db().await;

    let plan_id = create_test_plan(&pool).await;
    let task = create_test_task(&pool, plan_id, "human-task", "human_review", 3).await;

    let inv = create_test_invariant(&pool, "check_inv", "true", &[], 0).await;
    invariants::link_task_invariant(&pool, task.id, inv.id)
        .await
        .unwrap();

    advance_task_to_running(&pool, task.id, "/tmp").await;

    let runner = GateRunner::new(&pool);
    let verdict = runner.run_gate(task.id).await.expect("run_gate should succeed");
    assert!(matches!(verdict, GateVerdict::Passed));

    // Evaluate: should return HumanRequired, NOT auto-pass.
    let action = evaluate_verdict(&pool, task.id, &verdict)
        .await
        .expect("evaluate should succeed");
    assert_eq!(action, GateAction::HumanRequired);

    // Task should still be in checking state.
    let t = task_db::get_task(&pool, task.id).await.unwrap().unwrap();
    assert_eq!(t.status, TaskStatus::Checking);

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn human_approve_gate_leaves_task_in_checking() {
    let (pool, db_name) = create_temp_db().await;

    let plan_id = create_test_plan(&pool).await;
    let task = create_test_task(&pool, plan_id, "approve-task", "human_approve", 3).await;

    let inv = create_test_invariant(&pool, "approve_inv", "true", &[], 0).await;
    invariants::link_task_invariant(&pool, task.id, inv.id)
        .await
        .unwrap();

    advance_task_to_running(&pool, task.id, "/tmp").await;

    let runner = GateRunner::new(&pool);
    let verdict = runner.run_gate(task.id).await.expect("run_gate should succeed");
    assert!(matches!(verdict, GateVerdict::Passed));

    let action = evaluate_verdict(&pool, task.id, &verdict)
        .await
        .expect("evaluate should succeed");
    assert_eq!(action, GateAction::HumanRequired);

    let t = task_db::get_task(&pool, task.id).await.unwrap().unwrap();
    assert_eq!(t.status, TaskStatus::Checking);

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn gate_results_recorded_correctly() {
    let (pool, db_name) = create_temp_db().await;

    let plan_id = create_test_plan(&pool).await;
    let task = create_test_task(&pool, plan_id, "record-task", "auto", 3).await;

    let inv_pass = create_test_invariant(&pool, "rec_pass", "true", &[], 0).await;
    let inv_fail = create_test_invariant(&pool, "rec_fail", "false", &[], 0).await;

    invariants::link_task_invariant(&pool, task.id, inv_pass.id)
        .await
        .unwrap();
    invariants::link_task_invariant(&pool, task.id, inv_fail.id)
        .await
        .unwrap();

    advance_task_to_running(&pool, task.id, "/tmp").await;

    let runner = GateRunner::new(&pool);
    let _verdict = runner.run_gate(task.id).await.expect("run_gate should succeed");

    // Check that gate results were recorded.
    let results = gate_results::get_gate_results(&pool, task.id, 0)
        .await
        .expect("should get gate results");

    assert_eq!(results.len(), 2, "should have two gate results");

    // Find the passing and failing results.
    let pass_result = results.iter().find(|r| r.invariant_id == inv_pass.id);
    let fail_result = results.iter().find(|r| r.invariant_id == inv_fail.id);

    assert!(pass_result.is_some(), "should have a result for the passing invariant");
    assert!(fail_result.is_some(), "should have a result for the failing invariant");

    let pass_result = pass_result.unwrap();
    assert!(pass_result.passed);
    assert_eq!(pass_result.exit_code, Some(0));
    assert_eq!(pass_result.attempt, 0);
    assert!(pass_result.duration_ms.is_some());

    let fail_result = fail_result.unwrap();
    assert!(!fail_result.passed);
    assert_eq!(fail_result.exit_code, Some(1));
    assert_eq!(fail_result.attempt, 0);

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn gate_with_real_shell_commands() {
    let (pool, db_name) = create_temp_db().await;

    let plan_id = create_test_plan(&pool).await;
    let task = create_test_task(&pool, plan_id, "shell-task", "auto", 3).await;

    // An invariant that runs `echo hello` (produces stdout).
    let inv_echo = create_test_invariant(
        &pool,
        "echo_test",
        "echo",
        &["hello".to_owned()],
        0,
    )
    .await;

    // An invariant that runs `sh -c "echo err >&2 && exit 1"` (fails with stderr).
    let inv_stderr = create_test_invariant(
        &pool,
        "stderr_test",
        "sh",
        &["-c".to_owned(), "echo err >&2 && exit 1".to_owned()],
        0,
    )
    .await;

    invariants::link_task_invariant(&pool, task.id, inv_echo.id)
        .await
        .unwrap();
    invariants::link_task_invariant(&pool, task.id, inv_stderr.id)
        .await
        .unwrap();

    advance_task_to_running(&pool, task.id, "/tmp").await;

    let runner = GateRunner::new(&pool);
    let verdict = runner.run_gate(task.id).await.expect("run_gate should succeed");

    match &verdict {
        GateVerdict::Failed { failures } => {
            assert_eq!(failures.len(), 1);
            assert_eq!(failures[0].invariant_name, "stderr_test");
            assert_eq!(failures[0].exit_code, Some(1));
            assert!(
                failures[0].stderr_snippet.contains("err"),
                "stderr snippet should contain 'err', got: {:?}",
                failures[0].stderr_snippet
            );
        }
        GateVerdict::Passed => panic!("expected Failed verdict"),
    }

    // Verify the echo command's result was recorded with stdout.
    let results = gate_results::get_gate_results(&pool, task.id, 0)
        .await
        .unwrap();
    let echo_result = results
        .iter()
        .find(|r| r.invariant_id == inv_echo.id)
        .expect("should have echo result");
    assert!(echo_result.passed);
    assert!(
        echo_result.stdout.as_deref().unwrap_or("").contains("hello"),
        "stdout should contain 'hello'"
    );

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn auto_fail_retry_eligibility_when_max_reached() {
    let (pool, db_name) = create_temp_db().await;

    let plan_id = create_test_plan(&pool).await;
    // retry_max = 0 means no retries allowed.
    let task = create_test_task(&pool, plan_id, "no-retry-task", "auto", 0).await;

    let inv = create_test_invariant(&pool, "fail_nr", "false", &[], 0).await;
    invariants::link_task_invariant(&pool, task.id, inv.id)
        .await
        .unwrap();

    advance_task_to_running(&pool, task.id, "/tmp").await;

    let runner = GateRunner::new(&pool);
    let verdict = runner.run_gate(task.id).await.expect("run_gate should succeed");
    assert!(matches!(verdict, GateVerdict::Failed { .. }));

    let action = evaluate_verdict(&pool, task.id, &verdict)
        .await
        .expect("evaluate should succeed");
    assert_eq!(
        action,
        GateAction::AutoFailed { can_retry: false },
        "should not be eligible for retry when retry_max is 0"
    );

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn gate_runner_fails_if_no_invariants_linked() {
    let (pool, db_name) = create_temp_db().await;

    let plan_id = create_test_plan(&pool).await;
    let task = create_test_task(&pool, plan_id, "no-inv-task", "auto", 3).await;

    // No invariants linked.
    advance_task_to_running(&pool, task.id, "/tmp").await;

    let runner = GateRunner::new(&pool);
    let result = runner.run_gate(task.id).await;
    assert!(result.is_err(), "should fail with no invariants linked");
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("no linked invariants"),
        "error should mention no linked invariants: {err_msg}"
    );

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn gate_runner_requires_running_state() {
    let (pool, db_name) = create_temp_db().await;

    let plan_id = create_test_plan(&pool).await;
    let task = create_test_task(&pool, plan_id, "wrong-state", "auto", 3).await;

    let inv = create_test_invariant(&pool, "state_inv", "true", &[], 0).await;
    invariants::link_task_invariant(&pool, task.id, inv.id)
        .await
        .unwrap();

    // Task is still in pending state -- should fail to transition to checking.
    let runner = GateRunner::new(&pool);
    let result = runner.run_gate(task.id).await;
    assert!(
        result.is_err(),
        "should fail when task is not in running state"
    );

    pool.close().await;
    drop_temp_db(&db_name).await;
}
