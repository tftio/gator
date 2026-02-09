//! Fleet orchestration integration tests (T023).
//!
//! Tests the full orchestrator with a diamond DAG plan using a MockHarness
//! that produces configurable per-task behavior.

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use async_trait::async_trait;
use futures::Stream;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgPool};
use uuid::Uuid;

use gator_db::config::DbConfig;
use gator_db::models::{InvariantKind, InvariantScope, PlanStatus, TaskStatus};
use gator_db::pool;
use gator_db::queries::agent_events;
use gator_db::queries::invariants::{self, NewInvariant};
use gator_db::queries::plans as plan_db;
use gator_db::queries::tasks as task_db;

use gator_core::harness::types::{AgentEvent, AgentHandle, MaterializedTask};
use gator_core::harness::{Harness, HarnessRegistry};
use gator_core::isolation::{Isolation, worktree::WorktreeIsolation};
use gator_core::orchestrator::{run_orchestrator, OrchestratorConfig, OrchestratorResult};
use gator_core::token::TokenConfig;
use gator_core::worktree::WorktreeManager;

// ===========================================================================
// Test harness
// ===========================================================================

struct TestHarness {
    pool: PgPool,
    db_name: String,
    repo_dir: tempfile::TempDir,
    worktree_base_dir: tempfile::TempDir,
    repo_path: PathBuf,
}

impl TestHarness {
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

    fn pool(&self) -> &PgPool {
        &self.pool
    }

    fn worktree_base(&self) -> PathBuf {
        self.worktree_base_dir.path().to_path_buf()
    }

    fn worktree_manager(&self) -> WorktreeManager {
        WorktreeManager::new(&self.repo_path, Some(self.worktree_base()))
            .expect("failed to create WorktreeManager")
    }

    fn isolation(&self) -> Arc<dyn Isolation> {
        Arc::new(WorktreeIsolation::new(self.worktree_manager()))
    }

    async fn teardown(self) {
        self.pool.close().await;
        drop_temp_db(&self.db_name).await;
        drop(self.worktree_base_dir);
        drop(self.repo_dir);
    }
}

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
        .unwrap_or_else(|e| panic!("failed to create temp database: {e}"));
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
        .unwrap_or_else(|e| panic!("failed to connect to temp db: {e}"));

    let migrations_path = pool::default_migrations_path();
    pool::run_migrations(&temp_pool, migrations_path)
        .await
        .expect("migrations should succeed");

    (temp_pool, db_name)
}

async fn drop_temp_db(db_name: &str) {
    let base_config = DbConfig::from_env();
    let maint_url = base_config.maintenance_url();

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

fn test_token_config() -> TokenConfig {
    TokenConfig::new(b"fleet-test-secret-key".to_vec())
}

// ===========================================================================
// ConfigurableMockHarness
// ===========================================================================

/// Per-task behavior for the mock harness.
#[derive(Clone)]
enum TaskBehavior {
    /// Complete successfully with standard events.
    Complete,
    /// Hang forever (for timeout testing).
    Hang,
}

/// A mock harness where behavior can be configured per task name.
struct ConfigurableMockHarness {
    behaviors: Arc<Mutex<HashMap<String, TaskBehavior>>>,
    default_behavior: TaskBehavior,
    /// Map task_id -> task_name for resolving behavior in events().
    task_names: Arc<Mutex<HashMap<Uuid, String>>>,
    /// Track spawn order for topological assertions.
    spawn_log: Arc<Mutex<Vec<(String, chrono::DateTime<chrono::Utc>)>>>,
}

impl ConfigurableMockHarness {
    fn new(behaviors: HashMap<String, TaskBehavior>) -> Self {
        Self {
            behaviors: Arc::new(Mutex::new(behaviors)),
            default_behavior: TaskBehavior::Complete,
            task_names: Arc::new(Mutex::new(HashMap::new())),
            spawn_log: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl Harness for ConfigurableMockHarness {
    fn name(&self) -> &str {
        "mock-harness"
    }

    async fn spawn(&self, task: &MaterializedTask) -> Result<AgentHandle> {
        // Record spawn.
        self.task_names
            .lock()
            .unwrap()
            .insert(task.task_id, task.name.clone());
        self.spawn_log
            .lock()
            .unwrap()
            .push((task.name.clone(), chrono::Utc::now()));

        Ok(AgentHandle {
            pid: 99999,
            stdin: None,
            task_id: task.task_id,
            attempt: 0,
            harness_name: "mock-harness".to_string(),
        })
    }

    fn events(&self, handle: &AgentHandle) -> Pin<Box<dyn Stream<Item = AgentEvent> + Send>> {
        // Look up the task name by task_id (race-safe).
        let task_name = {
            let names = self.task_names.lock().unwrap();
            names.get(&handle.task_id).cloned().unwrap_or_default()
        };

        let behavior = {
            let behaviors = self.behaviors.lock().unwrap();
            behaviors
                .get(&task_name)
                .cloned()
                .unwrap_or(self.default_behavior.clone())
        };

        match behavior {
            TaskBehavior::Complete => {
                Box::pin(futures::stream::iter(vec![
                    AgentEvent::Message {
                        role: "assistant".to_string(),
                        content: format!("Working on {task_name}"),
                    },
                    AgentEvent::Completed,
                ]))
            }
            TaskBehavior::Hang => Box::pin(futures::stream::pending()),
        }
    }

    async fn send(&self, _handle: &AgentHandle, _message: &str) -> Result<()> {
        Ok(())
    }

    async fn kill(&self, _handle: &AgentHandle) -> Result<()> {
        Ok(())
    }

    async fn is_running(&self, _handle: &AgentHandle) -> bool {
        false
    }
}

// ===========================================================================
// Diamond DAG helper
// ===========================================================================

/// Create a diamond DAG plan:
///   foundation -> {api-layer, frontend} -> integration
///
/// All tasks use the given invariant. Returns (plan_id, task_ids_by_name).
async fn create_diamond_dag(
    pool: &PgPool,
    repo_path: &str,
    invariant_id: Uuid,
    retry_max: i32,
) -> (Uuid, HashMap<String, Uuid>) {
    let plan = plan_db::insert_plan(pool, "diamond-plan", repo_path, "main", None, "claude-code", "worktree")
        .await
        .expect("insert plan");

    plan_db::approve_plan(pool, plan.id)
        .await
        .expect("approve plan");

    let foundation = task_db::insert_task(
        pool,
        plan.id,
        "foundation",
        "Foundation task",
        "narrow",
        "auto",
        retry_max,
        None,
    )
    .await
    .expect("insert foundation");
    task_db::link_task_invariant(pool, foundation.id, invariant_id)
        .await
        .unwrap();

    let api_layer = task_db::insert_task(
        pool,
        plan.id,
        "api-layer",
        "API layer task",
        "narrow",
        "auto",
        retry_max,
        None,
    )
    .await
    .expect("insert api-layer");
    task_db::link_task_invariant(pool, api_layer.id, invariant_id)
        .await
        .unwrap();
    task_db::insert_task_dependency(pool, api_layer.id, foundation.id)
        .await
        .unwrap();

    let frontend = task_db::insert_task(
        pool,
        plan.id,
        "frontend",
        "Frontend task",
        "narrow",
        "auto",
        retry_max,
        None,
    )
    .await
    .expect("insert frontend");
    task_db::link_task_invariant(pool, frontend.id, invariant_id)
        .await
        .unwrap();
    task_db::insert_task_dependency(pool, frontend.id, foundation.id)
        .await
        .unwrap();

    let integration = task_db::insert_task(
        pool,
        plan.id,
        "integration",
        "Integration task",
        "narrow",
        "auto",
        retry_max,
        None,
    )
    .await
    .expect("insert integration");
    task_db::link_task_invariant(pool, integration.id, invariant_id)
        .await
        .unwrap();
    task_db::insert_task_dependency(pool, integration.id, api_layer.id)
        .await
        .unwrap();
    task_db::insert_task_dependency(pool, integration.id, frontend.id)
        .await
        .unwrap();

    let mut ids = HashMap::new();
    ids.insert("foundation".to_string(), foundation.id);
    ids.insert("api-layer".to_string(), api_layer.id);
    ids.insert("frontend".to_string(), frontend.id);
    ids.insert("integration".to_string(), integration.id);

    (plan.id, ids)
}

fn make_registry(harness: impl Harness + 'static) -> Arc<HarnessRegistry> {
    let mut registry = HarnessRegistry::new();
    registry.register(harness);
    Arc::new(registry)
}

// ===========================================================================
// Tests
// ===========================================================================

/// Test 1: All tasks succeed in topological order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn diamond_dag_topological_order() {
    let harness = TestHarness::new().await;
    let pool = harness.pool();

    let inv = invariants::insert_invariant(
        pool,
        &NewInvariant {
            name: "pass_inv",
            description: None,
            kind: InvariantKind::Custom,
            command: "true",
            args: &[],
            expected_exit_code: 0,
            threshold: None,
            scope: InvariantScope::Project,
            timeout_secs: 300,
        },
    )
    .await
    .expect("insert invariant");

    let (plan_id, task_ids) = create_diamond_dag(
        pool,
        &harness.repo_path.to_string_lossy(),
        inv.id,
        0,
    )
    .await;

    let mock = ConfigurableMockHarness::new(HashMap::new());
    let spawn_log = mock.spawn_log.clone();
    let registry = make_registry(mock);
    let isolation = harness.isolation();

    let result = run_orchestrator(
        pool,
        plan_id,
        &registry,
        &isolation,
        &test_token_config(),
        &OrchestratorConfig {
            max_agents: 4,
            task_timeout: Duration::from_secs(30),
        },
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(result, OrchestratorResult::Completed);

    // Verify all tasks passed.
    for (name, id) in &task_ids {
        let task = task_db::get_task(pool, *id).await.unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::Passed, "task {} should be passed", name);
    }

    // Verify topological order from spawn_log.
    let log = spawn_log.lock().unwrap().clone();
    let names: Vec<&str> = log.iter().map(|(n, _)| n.as_str()).collect();

    // Foundation must come before api-layer and frontend.
    let foundation_pos = names.iter().position(|&n| n == "foundation").unwrap();
    let api_pos = names.iter().position(|&n| n == "api-layer").unwrap();
    let frontend_pos = names.iter().position(|&n| n == "frontend").unwrap();
    let integration_pos = names.iter().position(|&n| n == "integration").unwrap();

    assert!(
        foundation_pos < api_pos,
        "foundation should start before api-layer"
    );
    assert!(
        foundation_pos < frontend_pos,
        "foundation should start before frontend"
    );
    assert!(
        integration_pos > api_pos,
        "integration should start after api-layer"
    );
    assert!(
        integration_pos > frontend_pos,
        "integration should start after frontend"
    );

    // Verify plan status.
    let plan = plan_db::get_plan(pool, plan_id).await.unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::Completed);

    harness.teardown().await;
}

/// Test 2: One task fails then succeeds on retry (using a failing invariant
/// that we swap to passing between attempts by relinking invariants).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_with_escalation() {
    let harness = TestHarness::new().await;
    let pool = harness.pool();

    // Failing invariant.
    let fail_inv = invariants::insert_invariant(
        pool,
        &NewInvariant {
            name: "fail_inv",
            description: None,
            kind: InvariantKind::Custom,
            command: "false",
            args: &[],
            expected_exit_code: 0,
            threshold: None,
            scope: InvariantScope::Project,
            timeout_secs: 300,
        },
    )
    .await
    .expect("insert invariant");

    let plan = plan_db::insert_plan(
        pool,
        "retry-plan",
        &harness.repo_path.to_string_lossy(),
        "main",
        None,
        "claude-code",
        "worktree",
    )
    .await
    .unwrap();
    plan_db::approve_plan(pool, plan.id).await.unwrap();

    let task = task_db::insert_task(
        pool,
        plan.id,
        "retry-task",
        "Will fail and retry",
        "narrow",
        "auto",
        1, // 1 retry allowed
        None,
    )
    .await
    .unwrap();
    task_db::link_task_invariant(pool, task.id, fail_inv.id)
        .await
        .unwrap();

    let mock = ConfigurableMockHarness::new(HashMap::new());
    let registry = make_registry(mock);
    let isolation = harness.isolation();

    let result = run_orchestrator(
        pool,
        plan.id,
        &registry,
        &isolation,
        &test_token_config(),
        &OrchestratorConfig {
            max_agents: 4,
            task_timeout: Duration::from_secs(30),
        },
        CancellationToken::new(),
    )
    .await
    .unwrap();

    // Should fail after retry exhaustion.
    match &result {
        OrchestratorResult::Failed { failed_tasks } => {
            assert!(
                failed_tasks.contains(&"retry-task".to_string()),
                "should contain retry-task: {:?}",
                failed_tasks
            );
        }
        other => panic!("expected Failed, got {:?}", other),
    }

    // Verify task was retried (attempt should be 1).
    let task_final = task_db::get_task(pool, task.id).await.unwrap().unwrap();
    assert_eq!(task_final.status, TaskStatus::Escalated);
    assert_eq!(task_final.attempt, 1);

    harness.teardown().await;
}

/// Test 3: Timeout kills an agent, retries, then escalates.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timeout_kills_agent() {
    let harness = TestHarness::new().await;
    let pool = harness.pool();

    let inv = invariants::insert_invariant(
        pool,
        &NewInvariant {
            name: "pass_inv",
            description: None,
            kind: InvariantKind::Custom,
            command: "true",
            args: &[],
            expected_exit_code: 0,
            threshold: None,
            scope: InvariantScope::Project,
            timeout_secs: 300,
        },
    )
    .await
    .expect("insert invariant");

    let plan = plan_db::insert_plan(
        pool,
        "timeout-plan",
        &harness.repo_path.to_string_lossy(),
        "main",
        None,
        "claude-code",
        "worktree",
    )
    .await
    .unwrap();
    plan_db::approve_plan(pool, plan.id).await.unwrap();

    let task = task_db::insert_task(
        pool,
        plan.id,
        "hanging-task",
        "Will hang and get killed",
        "narrow",
        "auto",
        0, // no retries
        None,
    )
    .await
    .unwrap();
    task_db::link_task_invariant(pool, task.id, inv.id)
        .await
        .unwrap();

    let mut behaviors = HashMap::new();
    behaviors.insert("hanging-task".to_string(), TaskBehavior::Hang);
    let mock = ConfigurableMockHarness::new(behaviors);
    let registry = make_registry(mock);
    let isolation = harness.isolation();

    let result = run_orchestrator(
        pool,
        plan.id,
        &registry,
        &isolation,
        &test_token_config(),
        &OrchestratorConfig {
            max_agents: 4,
            task_timeout: Duration::from_millis(200), // short timeout
        },
        CancellationToken::new(),
    )
    .await
    .unwrap();

    match &result {
        OrchestratorResult::Failed { failed_tasks } => {
            assert!(
                failed_tasks.contains(&"hanging-task".to_string()),
                "should contain hanging-task"
            );
        }
        other => panic!("expected Failed, got {:?}", other),
    }

    let task_final = task_db::get_task(pool, task.id).await.unwrap().unwrap();
    assert_eq!(task_final.status, TaskStatus::Escalated);

    harness.teardown().await;
}

/// Test 4: Restart recovery -- manually set a task to running, verify
/// orchestrator resets and retries it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_recovery() {
    let harness = TestHarness::new().await;
    let pool = harness.pool();

    let inv = invariants::insert_invariant(
        pool,
        &NewInvariant {
            name: "pass_inv",
            description: None,
            kind: InvariantKind::Custom,
            command: "true",
            args: &[],
            expected_exit_code: 0,
            threshold: None,
            scope: InvariantScope::Project,
            timeout_secs: 300,
        },
    )
    .await
    .expect("insert invariant");

    let plan = plan_db::insert_plan(
        pool,
        "restart-plan",
        &harness.repo_path.to_string_lossy(),
        "main",
        None,
        "claude-code",
        "worktree",
    )
    .await
    .unwrap();
    plan_db::approve_plan(pool, plan.id).await.unwrap();
    plan_db::update_plan_status(pool, plan.id, PlanStatus::Running)
        .await
        .unwrap();

    let task = task_db::insert_task(
        pool,
        plan.id,
        "orphan-task",
        "Was running when crash happened",
        "narrow",
        "auto",
        3,
        None,
    )
    .await
    .unwrap();
    task_db::link_task_invariant(pool, task.id, inv.id)
        .await
        .unwrap();

    // Simulate crash: manually set task to running state.
    task_db::assign_task_metadata(pool, task.id, "mock-harness", "/tmp/fake")
        .await
        .unwrap();
    task_db::transition_task_status(
        pool,
        task.id,
        TaskStatus::Pending,
        TaskStatus::Assigned,
        None,
        None,
    )
    .await
    .unwrap();
    task_db::transition_task_status(
        pool,
        task.id,
        TaskStatus::Assigned,
        TaskStatus::Running,
        Some(chrono::Utc::now()),
        None,
    )
    .await
    .unwrap();

    let mock = ConfigurableMockHarness::new(HashMap::new());
    let registry = make_registry(mock);
    let isolation = harness.isolation();

    let result = run_orchestrator(
        pool,
        plan.id,
        &registry,
        &isolation,
        &test_token_config(),
        &OrchestratorConfig {
            max_agents: 4,
            task_timeout: Duration::from_secs(30),
        },
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(result, OrchestratorResult::Completed);

    let task_final = task_db::get_task(pool, task.id).await.unwrap().unwrap();
    assert_eq!(task_final.status, TaskStatus::Passed);
    // Should have been retried (attempt incremented).
    assert_eq!(task_final.attempt, 1);

    harness.teardown().await;
}

/// Test 5: After orchestrator run, verify status and log data.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_and_log_after_run() {
    let harness = TestHarness::new().await;
    let pool = harness.pool();

    let inv = invariants::insert_invariant(
        pool,
        &NewInvariant {
            name: "pass_inv",
            description: None,
            kind: InvariantKind::Custom,
            command: "true",
            args: &[],
            expected_exit_code: 0,
            threshold: None,
            scope: InvariantScope::Project,
            timeout_secs: 300,
        },
    )
    .await
    .expect("insert invariant");

    let plan = plan_db::insert_plan(
        pool,
        "status-plan",
        &harness.repo_path.to_string_lossy(),
        "main",
        None,
        "claude-code",
        "worktree",
    )
    .await
    .unwrap();
    plan_db::approve_plan(pool, plan.id).await.unwrap();

    let task = task_db::insert_task(
        pool,
        plan.id,
        "status-task",
        "Task for status test",
        "narrow",
        "auto",
        0,
        None,
    )
    .await
    .unwrap();
    task_db::link_task_invariant(pool, task.id, inv.id)
        .await
        .unwrap();

    let mock = ConfigurableMockHarness::new(HashMap::new());
    let registry = make_registry(mock);
    let isolation = harness.isolation();

    let result = run_orchestrator(
        pool,
        plan.id,
        &registry,
        &isolation,
        &test_token_config(),
        &OrchestratorConfig {
            max_agents: 4,
            task_timeout: Duration::from_secs(30),
        },
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(result, OrchestratorResult::Completed);

    // Verify progress (status).
    let progress = task_db::get_plan_progress(pool, plan.id).await.unwrap();
    assert_eq!(progress.passed, 1);
    assert_eq!(progress.total, 1);

    // Verify events (log).
    let events = agent_events::list_events_for_task(pool, task.id, 0)
        .await
        .unwrap();
    assert!(
        !events.is_empty(),
        "should have at least some events recorded"
    );

    // Check that a "completed" event was recorded.
    let has_completed = events.iter().any(|e| e.event_type == "completed");
    assert!(has_completed, "should have a completed event");

    // Check a message event was recorded.
    let has_message = events.iter().any(|e| e.event_type == "message");
    assert!(has_message, "should have a message event");

    harness.teardown().await;
}
