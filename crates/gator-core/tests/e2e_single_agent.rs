//! End-to-end integration test for the single-agent dispatch cycle (T016).
//!
//! Exercises the full happy-path: init -> create invariants -> create plan ->
//! approve -> dispatch -> agent commands -> gate -> passed.
//!
//! Also includes a negative test: invariant failure -> task failed -> retry.
//!
//! Requirements:
//! - A running PostgreSQL instance (default: `postgresql://localhost:5432`)
//! - Git available on PATH
//!
//! Each test creates a unique temporary database and a temporary git
//! repository, both of which are cleaned up on completion (even on failure).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgPool};
use uuid::Uuid;

use gator_db::config::DbConfig;
use gator_db::models::{
    InvariantKind, InvariantScope, PlanStatus, TaskStatus,
};
use gator_db::pool;
use gator_db::queries::gate_results;
use gator_db::queries::invariants::{self, NewInvariant};
use gator_db::queries::plans as plan_db;
use gator_db::queries::tasks as task_db;

use gator_core::gate::evaluator::{evaluate_verdict, GateAction};
use gator_core::gate::{GateRunner, GateVerdict};
use gator_core::plan::{create_plan_from_toml, get_plan_with_tasks, materialize_task, parse_plan_toml};
use gator_core::state::dispatch;
use gator_core::state::queries as state_queries;
use gator_core::token::{self, TokenConfig};
use gator_core::token::guard;
use gator_core::worktree::WorktreeManager;

// ===========================================================================
// Test harness
// ===========================================================================

/// A self-cleaning test environment with a temporary database and git repo.
struct TestHarness {
    pool: PgPool,
    db_name: String,
    repo_dir: tempfile::TempDir,
    worktree_base_dir: tempfile::TempDir,
    repo_path: PathBuf,
}

impl TestHarness {
    /// Create a new harness: temp database (with migrations) + temp git repo
    /// (with an initial commit).
    async fn new() -> Self {
        let (pool, db_name) = create_temp_db().await;
        let (repo_dir, repo_path) = create_temp_git_repo();
        let worktree_base_dir =
            tempfile::TempDir::new().expect("failed to create worktree base dir");

        Self {
            pool,
            db_name,
            repo_dir,
            worktree_base_dir,
            repo_path,
        }
    }

    /// Return a reference to the database pool.
    fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Return the git repository path.
    fn repo_path(&self) -> &Path {
        &self.repo_path
    }

    /// Return the worktree base directory path.
    fn worktree_base(&self) -> PathBuf {
        self.worktree_base_dir.path().to_path_buf()
    }

    /// Create a WorktreeManager for this harness.
    fn worktree_manager(&self) -> WorktreeManager {
        WorktreeManager::new(&self.repo_path, Some(self.worktree_base()))
            .expect("failed to create WorktreeManager")
    }

    /// Clean up all resources.
    async fn teardown(self) {
        // Close the pool first to release all connections.
        self.pool.close().await;

        // Drop the temporary database.
        drop_temp_db(&self.db_name).await;

        // Temp directories are dropped automatically when `self` is dropped,
        // but we can be explicit.
        drop(self.worktree_base_dir);
        drop(self.repo_dir);
    }
}

/// Create a unique temporary database, run migrations, and return a pool.
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

    let migrations_path = pool::default_migrations_path();
    pool::run_migrations(&temp_pool, migrations_path)
        .await
        .expect("migrations should succeed");

    (temp_pool, db_name)
}

/// Drop a temporary database.
async fn drop_temp_db(db_name: &str) {
    let base_config = DbConfig::from_env();
    let maint_url = base_config.maintenance_url();

    let maint_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&maint_url)
        .await
        .expect("failed to connect to maintenance database for cleanup");

    // Terminate any remaining connections to the temp database.
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

/// Create a temporary git repository with an initial commit.
fn create_temp_git_repo() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::TempDir::new().expect("failed to create temp dir");
    let repo_path = dir.path().to_path_buf();

    let run = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(&repo_path)
            .output()
            .unwrap_or_else(|e| panic!("failed to run git {}: {e}", args.join(" ")));
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    };

    run(&["init"]);
    run(&["config", "user.email", "test@gator.dev"]);
    run(&["config", "user.name", "Gator Test"]);

    std::fs::write(repo_path.join("README.md"), "# Test repo\n")
        .expect("failed to write README");

    run(&["add", "."]);
    run(&["commit", "-m", "Initial commit"]);

    (dir, repo_path)
}

/// Insert an invariant into the database.
async fn insert_invariant(
    pool: &PgPool,
    name: &str,
    command: &str,
    args: &[String],
    expected_exit_code: i32,
) -> gator_db::models::Invariant {
    let new = NewInvariant {
        name,
        description: Some(&format!("test invariant: {name}")),
        kind: InvariantKind::Custom,
        command,
        args,
        expected_exit_code,
        threshold: None,
        scope: InvariantScope::Project,
        timeout_secs: 300,
    };
    invariants::insert_invariant(pool, &new)
        .await
        .unwrap_or_else(|e| panic!("failed to insert invariant {name}: {e}"))
}

/// Token configuration used for tests.
fn test_token_config() -> TokenConfig {
    TokenConfig::new(b"e2e-test-secret-key-for-hmac".to_vec())
}

// ===========================================================================
// Test 1: Happy-path full lifecycle
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_happy_path_single_agent_dispatch() {
    let harness = TestHarness::new().await;
    let pool = harness.pool();
    let token_config = test_token_config();

    // -----------------------------------------------------------------
    // Step 1: Bootstrap -- database is already migrated by the harness.
    // Verify tables exist.
    // -----------------------------------------------------------------
    let counts = pool::table_counts(pool)
        .await
        .expect("table_counts should succeed");
    assert!(
        !counts.is_empty(),
        "database should have tables after migration"
    );

    // -----------------------------------------------------------------
    // Step 2: Add invariants.
    // -----------------------------------------------------------------
    let echo_inv = insert_invariant(
        pool,
        "echo_test",
        "echo",
        &["ok".to_owned()],
        0,
    )
    .await;

    let _always_pass_inv =
        insert_invariant(pool, "always_pass", "true", &[], 0).await;

    // Verify invariants are in the database.
    let inv_list = invariants::list_invariants(pool)
        .await
        .expect("list_invariants should succeed");
    assert_eq!(inv_list.len(), 2);

    // -----------------------------------------------------------------
    // Step 3: Create a plan.toml with one narrow-scope task that
    //         references both invariants.
    // -----------------------------------------------------------------
    let plan_toml_content = r#"
[plan]
name = "e2e-test-plan"
base_branch = "main"

[[tasks]]
name = "e2e-task"
description = "End-to-end test task for single agent dispatch"
scope = "narrow"
gate = "auto"
retry_max = 3
depends_on = []
invariants = ["echo_test", "always_pass"]
"#;

    let plan_toml = parse_plan_toml(plan_toml_content)
        .expect("plan TOML should parse");

    let project_path = harness.repo_path().to_string_lossy().to_string();
    let (plan, warnings) =
        create_plan_from_toml(pool, &plan_toml, &project_path)
            .await
            .expect("create_plan_from_toml should succeed");

    assert!(
        warnings.is_empty(),
        "should have no warnings, got: {warnings:?}"
    );
    assert_eq!(plan.status, PlanStatus::Draft);

    // Verify plan + tasks.
    let (fetched_plan, tasks) = get_plan_with_tasks(pool, plan.id)
        .await
        .expect("get_plan_with_tasks should succeed");
    assert_eq!(fetched_plan.name, "e2e-test-plan");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].name, "e2e-task");
    assert_eq!(tasks[0].status, TaskStatus::Pending);

    let task_id = tasks[0].id;

    // Verify invariants are linked to the task.
    let linked_invariants = invariants::get_invariants_for_task(pool, task_id)
        .await
        .expect("get_invariants_for_task should succeed");
    assert_eq!(linked_invariants.len(), 2);

    // -----------------------------------------------------------------
    // Step 4: Approve the plan.
    // -----------------------------------------------------------------
    let approved_plan = plan_db::approve_plan(pool, plan.id)
        .await
        .expect("approve_plan should succeed");
    assert_eq!(approved_plan.status, PlanStatus::Approved);
    assert!(approved_plan.approved_at.is_some());

    // -----------------------------------------------------------------
    // Step 5: Simulate the dispatch cycle.
    // -----------------------------------------------------------------

    // 5a. Get ready tasks.
    let ready = state_queries::get_ready_tasks(pool, plan.id)
        .await
        .expect("get_ready_tasks should succeed");
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id, task_id);

    // 5b. Create a worktree for the task.
    let wt_manager = harness.worktree_manager();
    let branch_name = WorktreeManager::branch_name("e2e-test-plan", "e2e-task");
    let wt_info = wt_manager
        .create_worktree(&branch_name)
        .expect("create_worktree should succeed");

    assert!(wt_info.path.exists(), "worktree directory should exist");
    assert_eq!(wt_info.branch.as_deref(), Some(branch_name.as_str()));

    // 5c. Generate a scoped token.
    let attempt: u32 = 0;
    let agent_token =
        token::generate_token(&token_config, task_id, attempt);

    // Validate the token round-trips correctly.
    let claims = token::validate_token(&token_config, &agent_token)
        .expect("token should validate");
    assert_eq!(claims.task_id, task_id);
    assert_eq!(claims.attempt, attempt);

    // 5d. Assign the task (pending -> assigned).
    dispatch::assign_task(
        pool,
        task_id,
        "test-harness",
        &wt_info.path,
    )
    .await
    .expect("assign_task should succeed");

    let task = task_db::get_task(pool, task_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.status, TaskStatus::Assigned);
    assert_eq!(task.assigned_harness.as_deref(), Some("test-harness"));
    assert_eq!(
        task.worktree_path.as_deref(),
        Some(wt_info.path.to_string_lossy().as_ref())
    );

    // 5e. Start the task (assigned -> running).
    dispatch::start_task(pool, task_id)
        .await
        .expect("start_task should succeed");

    let task = task_db::get_task(pool, task_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.status, TaskStatus::Running);
    assert!(task.started_at.is_some());

    // -----------------------------------------------------------------
    // Step 6: Simulate agent commands.
    // -----------------------------------------------------------------

    // 6a. `gator task` equivalent: materialize the task description.
    //     In agent mode, the agent would call this via CLI; we call the
    //     library function directly.
    let task_md = materialize_task(pool, task_id)
        .await
        .expect("materialize_task should succeed");
    assert!(
        task_md.contains("e2e-task"),
        "task markdown should contain the task name"
    );
    assert!(
        task_md.contains("End-to-end test task"),
        "task markdown should contain the description"
    );
    assert!(
        task_md.contains("echo_test"),
        "task markdown should list the echo_test invariant"
    );
    assert!(
        task_md.contains("always_pass"),
        "task markdown should list the always_pass invariant"
    );

    // 6b. `gator check` equivalent: run the gate.
    let gate_runner = GateRunner::new(pool);
    let verdict = gate_runner
        .run_gate(task_id)
        .await
        .expect("run_gate should succeed");

    // All invariants should pass.
    assert!(
        matches!(verdict, GateVerdict::Passed),
        "expected GateVerdict::Passed, got {:?}",
        verdict
    );

    // Task is now in 'checking' state.
    let task = task_db::get_task(pool, task_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.status, TaskStatus::Checking);

    // 6c. `gator done` equivalent: evaluate the verdict.
    let action = evaluate_verdict(pool, task_id, &verdict)
        .await
        .expect("evaluate_verdict should succeed");
    assert_eq!(action, GateAction::AutoPassed);

    // Task should now be 'passed'.
    let task = task_db::get_task(pool, task_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.status, TaskStatus::Passed);
    assert!(task.completed_at.is_some());

    // -----------------------------------------------------------------
    // Step 7: Verify operator commands rejected with agent token.
    // -----------------------------------------------------------------

    // The guard system uses environment variables. We test the inner
    // (env-free) functions to avoid env var race conditions with other
    // tests.

    // With a token present, operator mode should be blocked.
    let op_result = guard::require_operator_mode_with_value(Some(agent_token.clone()));
    assert!(
        op_result.is_err(),
        "operator mode should be blocked when agent token is present"
    );

    // With a valid token, agent mode should succeed.
    let agent_result = guard::require_agent_mode_with_value(&token_config, &agent_token);
    assert!(
        agent_result.is_ok(),
        "agent mode should succeed with valid token"
    );
    let agent_claims = agent_result.unwrap();
    assert_eq!(agent_claims.task_id, task_id);
    assert_eq!(agent_claims.attempt, attempt);

    // Without a token, operator mode should succeed.
    let op_result_no_token = guard::require_operator_mode_with_value(None);
    assert!(
        op_result_no_token.is_ok(),
        "operator mode should succeed without agent token"
    );

    // -----------------------------------------------------------------
    // Step 8: Verify gate results are recorded in the DB.
    // -----------------------------------------------------------------
    let gate_results_list = gate_results::get_gate_results(pool, task_id, 0)
        .await
        .expect("get_gate_results should succeed");

    assert_eq!(
        gate_results_list.len(),
        2,
        "should have 2 gate results (one per invariant)"
    );

    // Both should have passed.
    for gr in &gate_results_list {
        assert!(gr.passed, "gate result should show passed");
        assert_eq!(gr.exit_code, Some(0));
        assert_eq!(gr.attempt, 0);
        assert!(gr.duration_ms.is_some());
    }

    // Verify the echo_test invariant captured stdout.
    let echo_result = gate_results_list
        .iter()
        .find(|gr| gr.invariant_id == echo_inv.id)
        .expect("should find echo_test gate result");
    assert!(
        echo_result
            .stdout
            .as_deref()
            .unwrap_or("")
            .contains("ok"),
        "echo_test stdout should contain 'ok'"
    );

    // -----------------------------------------------------------------
    // Step 9: Verify plan progress and completion.
    // -----------------------------------------------------------------
    let progress = state_queries::get_plan_progress(pool, plan.id)
        .await
        .expect("get_plan_progress should succeed");
    assert_eq!(progress.passed, 1);
    assert_eq!(progress.total, 1);

    let is_complete = state_queries::is_plan_complete(pool, plan.id)
        .await
        .expect("is_plan_complete should succeed");
    assert!(is_complete, "plan should be complete");

    // -----------------------------------------------------------------
    // Step 10: Clean up worktree.
    // -----------------------------------------------------------------
    wt_manager
        .remove_worktree(&wt_info.path)
        .expect("remove_worktree should succeed");
    assert!(
        !wt_info.path.exists(),
        "worktree directory should be removed"
    );

    // -----------------------------------------------------------------
    // Teardown.
    // -----------------------------------------------------------------
    harness.teardown().await;
}

// ===========================================================================
// Test 2: Negative test -- invariant failure, retry, then pass
// ===========================================================================

#[tokio::test]
async fn e2e_invariant_failure_retry_then_pass() {
    let harness = TestHarness::new().await;
    let pool = harness.pool();

    // -----------------------------------------------------------------
    // Setup: Create invariants -- one that passes, one that fails.
    // -----------------------------------------------------------------
    let pass_inv =
        insert_invariant(pool, "always_true", "true", &[], 0).await;
    let fail_inv =
        insert_invariant(pool, "always_false", "false", &[], 0).await;

    // -----------------------------------------------------------------
    // Create plan with a task linked to BOTH invariants (so gate fails).
    // -----------------------------------------------------------------
    let plan_toml_content = r#"
[plan]
name = "e2e-negative-plan"
base_branch = "main"

[[tasks]]
name = "fail-task"
description = "Task that will fail due to always_false invariant"
scope = "narrow"
gate = "auto"
retry_max = 2
depends_on = []
invariants = ["always_true", "always_false"]
"#;

    let plan_toml = parse_plan_toml(plan_toml_content)
        .expect("plan TOML should parse");

    let project_path = harness.repo_path().to_string_lossy().to_string();
    let (plan, warnings) =
        create_plan_from_toml(pool, &plan_toml, &project_path)
            .await
            .expect("create_plan_from_toml should succeed");
    assert!(warnings.is_empty());

    // Approve the plan.
    plan_db::approve_plan(pool, plan.id)
        .await
        .expect("approve should succeed");

    let (_, tasks) = get_plan_with_tasks(pool, plan.id).await.unwrap();
    let task_id = tasks[0].id;

    // Create worktree.
    let wt_manager = harness.worktree_manager();
    let branch_name = WorktreeManager::branch_name("e2e-negative-plan", "fail-task");
    let wt_info = wt_manager
        .create_worktree(&branch_name)
        .expect("create_worktree should succeed");

    // -----------------------------------------------------------------
    // First attempt: dispatch and run gate (should fail).
    // -----------------------------------------------------------------
    dispatch::assign_task(pool, task_id, "test-harness", &wt_info.path)
        .await
        .expect("assign should succeed");
    dispatch::start_task(pool, task_id)
        .await
        .expect("start should succeed");

    let gate_runner = GateRunner::new(pool);
    let verdict = gate_runner
        .run_gate(task_id)
        .await
        .expect("run_gate should succeed");

    // Verdict should be Failed.
    match &verdict {
        GateVerdict::Failed { failures } => {
            assert_eq!(failures.len(), 1, "should have exactly one failure");
            assert_eq!(failures[0].invariant_name, "always_false");
            assert_eq!(failures[0].exit_code, Some(1));
        }
        GateVerdict::Passed => {
            panic!("expected Failed verdict, got Passed")
        }
    }

    // Evaluate: should auto-fail, can_retry = true.
    let action = evaluate_verdict(pool, task_id, &verdict)
        .await
        .expect("evaluate should succeed");
    assert_eq!(action, GateAction::AutoFailed { can_retry: true });

    // Task should be in 'failed' state.
    let task = task_db::get_task(pool, task_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.status, TaskStatus::Failed);
    assert_eq!(task.attempt, 0);

    // Verify gate results for attempt 0 were recorded.
    let results_attempt_0 = gate_results::get_gate_results(pool, task_id, 0)
        .await
        .unwrap();
    assert_eq!(results_attempt_0.len(), 2);

    let fail_result = results_attempt_0
        .iter()
        .find(|r| r.invariant_id == fail_inv.id)
        .unwrap();
    assert!(!fail_result.passed);
    assert_eq!(fail_result.exit_code, Some(1));

    let pass_result = results_attempt_0
        .iter()
        .find(|r| r.invariant_id == pass_inv.id)
        .unwrap();
    assert!(pass_result.passed);
    assert_eq!(pass_result.exit_code, Some(0));

    // -----------------------------------------------------------------
    // Retry: failed -> assigned (attempt incremented).
    // -----------------------------------------------------------------
    dispatch::retry_task(pool, task_id)
        .await
        .expect("retry should succeed");

    let task = task_db::get_task(pool, task_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.status, TaskStatus::Assigned);
    assert_eq!(task.attempt, 1);
    assert!(task.started_at.is_none(), "started_at should be cleared on retry");
    assert!(task.completed_at.is_none(), "completed_at should be cleared on retry");

    // -----------------------------------------------------------------
    // Second attempt: fix the problem by unlinking the failing invariant,
    // then re-run. This simulates an agent fixing the issue.
    // -----------------------------------------------------------------

    // Unlink the failing invariant and link only the passing one.
    // (In reality, the invariant definition wouldn't change, but the task
    // code would be fixed. Here we simulate success by removing the
    // failing check.)
    sqlx::query("DELETE FROM task_invariants WHERE task_id = $1 AND invariant_id = $2")
        .bind(task_id)
        .bind(fail_inv.id)
        .execute(pool)
        .await
        .expect("should unlink failing invariant");

    // Re-start the task (assigned -> running).
    dispatch::start_task(pool, task_id)
        .await
        .expect("start should succeed on retry");

    // Run gate again.
    let verdict_2 = gate_runner
        .run_gate(task_id)
        .await
        .expect("run_gate should succeed on retry");

    assert!(
        matches!(verdict_2, GateVerdict::Passed),
        "expected Passed on retry, got {:?}",
        verdict_2
    );

    // Evaluate.
    let action_2 = evaluate_verdict(pool, task_id, &verdict_2)
        .await
        .expect("evaluate should succeed on retry");
    assert_eq!(action_2, GateAction::AutoPassed);

    // Task should be passed.
    let task = task_db::get_task(pool, task_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.status, TaskStatus::Passed);
    assert!(task.completed_at.is_some());

    // Verify gate results for attempt 1 were recorded.
    let results_attempt_1 = gate_results::get_gate_results(pool, task_id, 1)
        .await
        .unwrap();
    assert_eq!(
        results_attempt_1.len(),
        1,
        "should have 1 gate result on retry (only the passing invariant)"
    );
    assert!(results_attempt_1[0].passed);

    // Plan should be complete.
    let is_complete = state_queries::is_plan_complete(pool, plan.id)
        .await
        .unwrap();
    assert!(is_complete);

    // Clean up worktree.
    wt_manager
        .remove_worktree(&wt_info.path)
        .expect("remove_worktree should succeed");

    harness.teardown().await;
}

// ===========================================================================
// Test 3: Pure failure with no retry (retry_max = 0)
// ===========================================================================

#[tokio::test]
async fn e2e_invariant_failure_no_retry() {
    let harness = TestHarness::new().await;
    let pool = harness.pool();

    // Create a failing invariant.
    let _fail_inv =
        insert_invariant(pool, "will_fail", "false", &[], 0).await;

    let plan_toml_content = r#"
[plan]
name = "e2e-no-retry-plan"
base_branch = "main"

[[tasks]]
name = "no-retry-task"
description = "Task that fails and cannot retry"
scope = "narrow"
gate = "auto"
retry_max = 0
depends_on = []
invariants = ["will_fail"]
"#;

    let plan_toml = parse_plan_toml(plan_toml_content).unwrap();
    let project_path = harness.repo_path().to_string_lossy().to_string();
    let (plan, _) = create_plan_from_toml(pool, &plan_toml, &project_path)
        .await
        .unwrap();
    plan_db::approve_plan(pool, plan.id).await.unwrap();

    let (_, tasks) = get_plan_with_tasks(pool, plan.id).await.unwrap();
    let task_id = tasks[0].id;

    // Create worktree.
    let wt_manager = harness.worktree_manager();
    let branch_name = WorktreeManager::branch_name("e2e-no-retry-plan", "no-retry-task");
    let wt_info = wt_manager
        .create_worktree(&branch_name)
        .expect("create_worktree should succeed");

    // Dispatch and run.
    dispatch::assign_task(pool, task_id, "test-harness", &wt_info.path)
        .await
        .unwrap();
    dispatch::start_task(pool, task_id).await.unwrap();

    let gate_runner = GateRunner::new(pool);
    let verdict = gate_runner.run_gate(task_id).await.unwrap();
    assert!(matches!(verdict, GateVerdict::Failed { .. }));

    // Evaluate: should auto-fail, can_retry = false.
    let action = evaluate_verdict(pool, task_id, &verdict)
        .await
        .unwrap();
    assert_eq!(action, GateAction::AutoFailed { can_retry: false });

    // Task should be failed.
    let task = task_db::get_task(pool, task_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.status, TaskStatus::Failed);

    // Retry should fail (attempt 0 >= retry_max 0).
    let retry_result = dispatch::retry_task(pool, task_id).await;
    assert!(retry_result.is_err(), "retry should fail when retry_max = 0");
    let err_msg = format!("{}", retry_result.unwrap_err());
    assert!(
        err_msg.contains("retry_max"),
        "error should mention retry_max: {err_msg}"
    );

    // Clean up.
    wt_manager.remove_worktree(&wt_info.path).unwrap();
    harness.teardown().await;
}
