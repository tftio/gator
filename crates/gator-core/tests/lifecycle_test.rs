//! Tests for the agent lifecycle manager (T019).
//!
//! Uses a MockHarness that produces configurable event sequences without
//! spawning real subprocesses.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgPool};
use uuid::Uuid;

use gator_db::config::DbConfig;
use gator_db::models::{InvariantKind, InvariantScope, TaskStatus};
use gator_db::pool;
use gator_db::queries::agent_events;
use gator_db::queries::invariants::{self, NewInvariant};
use gator_db::queries::plans as plan_db;
use gator_db::queries::tasks as task_db;

use gator_core::harness::types::{AgentEvent, AgentHandle, MaterializedTask};
use gator_core::harness::Harness;
use gator_core::lifecycle::{run_agent_lifecycle, LifecycleConfig, LifecycleResult};
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
    TokenConfig::new(b"lifecycle-test-secret-key".to_vec())
}

// ===========================================================================
// MockHarness
// ===========================================================================

/// Configurable behavior for the mock harness.
#[derive(Clone)]
enum MockBehavior {
    /// Emit some events then complete normally.
    Complete { events: Vec<AgentEvent> },
    /// Hang forever (for timeout testing).
    Hang,
}

struct MockHarness {
    behavior: MockBehavior,
    /// Track whether work was done (e.g., write a marker file in the worktree).
    do_work: bool,
}

impl MockHarness {
    fn completing(events: Vec<AgentEvent>, do_work: bool) -> Self {
        Self {
            behavior: MockBehavior::Complete { events },
            do_work,
        }
    }

    fn hanging() -> Self {
        Self {
            behavior: MockBehavior::Hang,
            do_work: false,
        }
    }
}

#[async_trait]
impl Harness for MockHarness {
    fn name(&self) -> &str {
        "mock-harness"
    }

    async fn spawn(&self, task: &MaterializedTask) -> Result<AgentHandle> {
        // If do_work is true, create a marker file in the working directory.
        if self.do_work {
            let marker = task.working_dir.join("agent-work.txt");
            std::fs::write(&marker, "work done\n").ok();
        }

        Ok(AgentHandle {
            pid: 99999,
            stdin: None,
            task_id: task.task_id,
            attempt: 0,
            harness_name: "mock-harness".to_string(),
        })
    }

    fn events(&self, _handle: &AgentHandle) -> Pin<Box<dyn Stream<Item = AgentEvent> + Send>> {
        match &self.behavior {
            MockBehavior::Complete { events } => {
                let events = events.clone();
                Box::pin(futures::stream::iter(events))
            }
            MockBehavior::Hang => {
                // Stream that never yields.
                Box::pin(futures::stream::pending())
            }
        }
    }

    async fn send(&self, _handle: &AgentHandle, _message: &str) -> Result<()> {
        Ok(())
    }

    async fn kill(&self, _handle: &AgentHandle) -> Result<()> {
        Ok(())
    }

    async fn is_running(&self, _handle: &AgentHandle) -> bool {
        matches!(self.behavior, MockBehavior::Hang)
    }
}

// ===========================================================================
// Helpers
// ===========================================================================

/// Set up a plan with one task linked to an always-passing invariant.
async fn setup_passing_task(pool: &PgPool, repo_path: &Path) -> (Uuid, gator_db::models::Task) {
    let plan = plan_db::insert_plan(
        pool,
        "lifecycle-plan",
        &repo_path.to_string_lossy(),
        "main",
    )
    .await
    .expect("insert plan");

    plan_db::approve_plan(pool, plan.id)
        .await
        .expect("approve plan");

    let task = task_db::insert_task(
        pool,
        plan.id,
        "lifecycle-task",
        "A test task for lifecycle",
        "narrow",
        "auto",
        3,
    )
    .await
    .expect("insert task");

    // Always-passing invariant.
    let inv = invariants::insert_invariant(
        pool,
        &NewInvariant {
            name: "always_pass",
            description: None,
            kind: InvariantKind::Custom,
            command: "true",
            args: &[],
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

    (plan.id, task)
}

/// Set up a plan with one task linked to a failing invariant.
async fn setup_failing_task(
    pool: &PgPool,
    repo_path: &Path,
    retry_max: i32,
) -> (Uuid, gator_db::models::Task) {
    let plan = plan_db::insert_plan(
        pool,
        "lifecycle-fail-plan",
        &repo_path.to_string_lossy(),
        "main",
    )
    .await
    .expect("insert plan");

    plan_db::approve_plan(pool, plan.id)
        .await
        .expect("approve plan");

    let task = task_db::insert_task(
        pool,
        plan.id,
        "lifecycle-fail-task",
        "A task that will fail",
        "narrow",
        "auto",
        retry_max,
    )
    .await
    .expect("insert task");

    // Always-failing invariant.
    let inv = invariants::insert_invariant(
        pool,
        &NewInvariant {
            name: "always_fail",
            description: None,
            kind: InvariantKind::Custom,
            command: "false",
            args: &[],
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

    (plan.id, task)
}

// ===========================================================================
// Tests
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn happy_path_lifecycle_passes() {
    let harness = TestHarness::new().await;
    let pool = harness.pool();

    let (_plan_id, task) = setup_passing_task(pool, &harness.repo_path).await;

    let mock = MockHarness::completing(
        vec![
            AgentEvent::Message {
                role: "assistant".to_string(),
                content: "Working on it".to_string(),
            },
            AgentEvent::Completed,
        ],
        false,
    );

    let result = run_agent_lifecycle(
        pool,
        &task,
        "lifecycle-plan",
        &mock,
        &harness.worktree_manager(),
        &test_token_config(),
        &LifecycleConfig {
            timeout: Duration::from_secs(30),
        },
    )
    .await
    .expect("lifecycle should succeed");

    assert_eq!(result, LifecycleResult::Passed);

    // Verify task is now passed.
    let updated = task_db::get_task(pool, task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.status, TaskStatus::Passed);

    harness.teardown().await;
}

#[tokio::test]
async fn failing_invariant_with_retries_returns_failed_can_retry() {
    let harness = TestHarness::new().await;
    let pool = harness.pool();

    let (_plan_id, task) = setup_failing_task(pool, &harness.repo_path, 3).await;

    let mock = MockHarness::completing(
        vec![AgentEvent::Completed],
        false,
    );

    let result = run_agent_lifecycle(
        pool,
        &task,
        "lifecycle-fail-plan",
        &mock,
        &harness.worktree_manager(),
        &test_token_config(),
        &LifecycleConfig {
            timeout: Duration::from_secs(30),
        },
    )
    .await
    .expect("lifecycle should succeed");

    assert_eq!(result, LifecycleResult::FailedCanRetry);

    let updated = task_db::get_task(pool, task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.status, TaskStatus::Failed);

    harness.teardown().await;
}

#[tokio::test]
async fn failing_invariant_no_retries_returns_failed_no_retry() {
    let harness = TestHarness::new().await;
    let pool = harness.pool();

    let (_plan_id, task) = setup_failing_task(pool, &harness.repo_path, 0).await;

    let mock = MockHarness::completing(
        vec![AgentEvent::Completed],
        false,
    );

    let result = run_agent_lifecycle(
        pool,
        &task,
        "lifecycle-fail-plan",
        &mock,
        &harness.worktree_manager(),
        &test_token_config(),
        &LifecycleConfig {
            timeout: Duration::from_secs(30),
        },
    )
    .await
    .expect("lifecycle should succeed");

    assert_eq!(result, LifecycleResult::FailedNoRetry);

    harness.teardown().await;
}

#[tokio::test]
async fn timeout_returns_timed_out() {
    let harness = TestHarness::new().await;
    let pool = harness.pool();

    let (_plan_id, task) = setup_passing_task(pool, &harness.repo_path).await;

    let mock = MockHarness::hanging();

    let result = run_agent_lifecycle(
        pool,
        &task,
        "lifecycle-plan",
        &mock,
        &harness.worktree_manager(),
        &test_token_config(),
        &LifecycleConfig {
            timeout: Duration::from_millis(100),
        },
    )
    .await
    .expect("lifecycle should succeed even on timeout");

    assert_eq!(result, LifecycleResult::TimedOut);

    // Task should be failed after timeout.
    let updated = task_db::get_task(pool, task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.status, TaskStatus::Failed);

    harness.teardown().await;
}

#[tokio::test]
async fn events_persisted_to_db() {
    let harness = TestHarness::new().await;
    let pool = harness.pool();

    let (_plan_id, task) = setup_passing_task(pool, &harness.repo_path).await;

    let mock = MockHarness::completing(
        vec![
            AgentEvent::Message {
                role: "assistant".to_string(),
                content: "Starting work".to_string(),
            },
            AgentEvent::ToolCall {
                tool: "Bash".to_string(),
                input: serde_json::json!({"command": "ls"}),
            },
            AgentEvent::TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
            },
            AgentEvent::Completed,
        ],
        false,
    );

    let _result = run_agent_lifecycle(
        pool,
        &task,
        "lifecycle-plan",
        &mock,
        &harness.worktree_manager(),
        &test_token_config(),
        &LifecycleConfig {
            timeout: Duration::from_secs(30),
        },
    )
    .await
    .expect("lifecycle should succeed");

    // Verify events were persisted.
    let events = agent_events::list_events_for_task(pool, task.id, 0)
        .await
        .expect("list events should succeed");

    // We expect 4 events: message, tool_call, token_usage, completed.
    assert_eq!(events.len(), 4, "should have 4 persisted events");
    assert_eq!(events[0].event_type, "message");
    assert_eq!(events[1].event_type, "tool_call");
    assert_eq!(events[2].event_type, "token_usage");
    assert_eq!(events[3].event_type, "completed");

    harness.teardown().await;
}
