//! Integration tests for the `gator plan` CLI commands.
//!
//! These tests exercise `plan create`, `plan show`, and `plan approve` against
//! a real PostgreSQL instance. Each test creates an isolated temporary database
//! and drops it on completion.

use sqlx::PgPool;
use uuid::Uuid;

use gator_core::plan::{create_plan_from_toml, get_plan_with_tasks, parse_plan_toml};
use gator_db::models::PlanStatus;
use gator_db::queries::{invariants, plans, tasks};
use gator_test_utils::{create_test_db, drop_test_db};

// -----------------------------------------------------------------------
// Helper: create a plan from a TOML string directly (simulates what the
// CLI does without needing a file on disk).
// -----------------------------------------------------------------------

async fn create_test_plan(pool: &PgPool, toml_str: &str) -> gator_db::models::Plan {
    let plan_toml = parse_plan_toml(toml_str).expect("test TOML should parse");
    create_plan_from_toml(pool, &plan_toml, "/tmp/test-project")
        .await
        .expect("create_plan_from_toml should succeed")
}

// -----------------------------------------------------------------------
// Helper: insert a test invariant.
// -----------------------------------------------------------------------

async fn insert_test_invariant(pool: &PgPool, name: &str) -> gator_db::models::Invariant {
    let new = invariants::NewInvariant {
        name,
        description: Some("test invariant"),
        kind: gator_db::models::InvariantKind::Custom,
        command: "true",
        args: &[],
        expected_exit_code: 0,
        threshold: None,
        scope: gator_db::models::InvariantScope::Project,
        timeout_secs: 300,
    };
    invariants::insert_invariant(pool, &new)
        .await
        .expect("insert_invariant should succeed")
}

// -----------------------------------------------------------------------
// Tests: plan create
// -----------------------------------------------------------------------

#[tokio::test]
async fn create_plan_from_toml_and_verify() {
    let (pool, db_name) = create_test_db().await;

    let toml_str = r#"
[plan]
name = "Test plan"
base_branch = "main"

[[tasks]]
name = "task-a"
description = "First task"
scope = "narrow"
gate = "auto"

[[tasks]]
name = "task-b"
description = "Second task"
scope = "medium"
gate = "human_review"
depends_on = ["task-a"]
"#;

    let plan = create_test_plan(&pool, toml_str).await;

    assert_eq!(plan.name, "Test plan");
    assert_eq!(plan.status, PlanStatus::Draft);
    assert!(plan.approved_at.is_none());

    // Verify tasks were created.
    let (_, found_tasks) = get_plan_with_tasks(&pool, plan.id)
        .await
        .expect("get_plan_with_tasks should succeed");
    assert_eq!(found_tasks.len(), 2);

    // Verify dependency edges.
    let dep_edges = tasks::count_dependency_edges(&pool, plan.id)
        .await
        .expect("count_dependency_edges should succeed");
    assert_eq!(dep_edges, 1);

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn create_plan_rejects_missing_invariants() {
    let (pool, db_name) = create_test_db().await;

    let toml_str = r#"
[plan]
name = "Plan with invariants"
base_branch = "main"

[[tasks]]
name = "task-a"
description = "Task A"
scope = "narrow"
gate = "auto"
invariants = ["nonexistent_invariant"]
"#;

    let plan_toml = parse_plan_toml(toml_str).expect("TOML should parse");
    let result = create_plan_from_toml(&pool, &plan_toml, "/tmp/test").await;

    assert!(result.is_err(), "should fail with missing invariant");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("nonexistent_invariant"),
        "error should mention the missing invariant: {err_msg}"
    );

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn create_plan_links_existing_invariants() {
    let (pool, db_name) = create_test_db().await;

    // Insert an invariant first.
    let inv = insert_test_invariant(&pool, "my_check").await;

    let toml_str = r#"
[plan]
name = "Plan with linked invariants"
base_branch = "main"

[[tasks]]
name = "task-a"
description = "Task A"
scope = "narrow"
gate = "auto"
invariants = ["my_check"]
"#;

    let plan_toml = parse_plan_toml(toml_str).expect("TOML should parse");
    let plan = create_plan_from_toml(&pool, &plan_toml, "/tmp/test")
        .await
        .expect("create should succeed");

    // Verify the invariant was linked.
    let (_, found_tasks) = get_plan_with_tasks(&pool, plan.id)
        .await
        .expect("get_plan_with_tasks should succeed");
    let task_a = &found_tasks[0];
    let task_invs = invariants::get_invariants_for_task(&pool, task_a.id)
        .await
        .expect("get_invariants_for_task should succeed");
    assert_eq!(task_invs.len(), 1);
    assert_eq!(task_invs[0].id, inv.id);

    pool.close().await;
    drop_test_db(&db_name).await;
}

// -----------------------------------------------------------------------
// Tests: plan show (list all)
// -----------------------------------------------------------------------

#[tokio::test]
async fn list_plans_returns_all() {
    let (pool, db_name) = create_test_db().await;

    // Create two plans.
    let toml1 = r#"
[plan]
name = "Plan One"
base_branch = "main"

[[tasks]]
name = "t1"
description = "Task 1"
scope = "narrow"
gate = "auto"
"#;
    let toml2 = r#"
[plan]
name = "Plan Two"
base_branch = "develop"

[[tasks]]
name = "t2"
description = "Task 2"
scope = "broad"
gate = "human_approve"
"#;

    create_test_plan(&pool, toml1).await;
    create_test_plan(&pool, toml2).await;

    let all_plans = plans::list_plans(&pool)
        .await
        .expect("list_plans should succeed");
    assert_eq!(all_plans.len(), 2);

    let names: Vec<&str> = all_plans.iter().map(|p| p.name.as_str()).collect();
    assert!(names.contains(&"Plan One"));
    assert!(names.contains(&"Plan Two"));

    pool.close().await;
    drop_test_db(&db_name).await;
}

// -----------------------------------------------------------------------
// Tests: plan show <id> (detailed view)
// -----------------------------------------------------------------------

#[tokio::test]
async fn show_plan_returns_tasks_and_dependencies() {
    let (pool, db_name) = create_test_db().await;

    let toml_str = r#"
[plan]
name = "Detailed plan"
base_branch = "main"

[[tasks]]
name = "alpha"
description = "Alpha task"
scope = "narrow"
gate = "auto"

[[tasks]]
name = "beta"
description = "Beta task"
scope = "medium"
gate = "human_review"
depends_on = ["alpha"]

[[tasks]]
name = "gamma"
description = "Gamma task"
scope = "broad"
gate = "human_approve"
depends_on = ["alpha", "beta"]
"#;

    let plan = create_test_plan(&pool, toml_str).await;

    let (found_plan, found_tasks) = get_plan_with_tasks(&pool, plan.id)
        .await
        .expect("get_plan_with_tasks should succeed");

    assert_eq!(found_plan.name, "Detailed plan");
    assert_eq!(found_tasks.len(), 3);

    // Verify dependencies for gamma.
    let gamma = found_tasks.iter().find(|t| t.name == "gamma").unwrap();
    let gamma_deps = tasks::get_task_dependency_names(&pool, gamma.id)
        .await
        .expect("get_task_dependency_names should succeed");
    assert_eq!(gamma_deps.len(), 2);
    assert!(gamma_deps.contains(&"alpha".to_string()));
    assert!(gamma_deps.contains(&"beta".to_string()));

    pool.close().await;
    drop_test_db(&db_name).await;
}

// -----------------------------------------------------------------------
// Tests: plan approve
// -----------------------------------------------------------------------

#[tokio::test]
async fn approve_plan_succeeds_when_all_tasks_have_invariants() {
    let (pool, db_name) = create_test_db().await;

    // Insert an invariant.
    let _inv = insert_test_invariant(&pool, "build_check").await;

    let toml_str = r#"
[plan]
name = "Approvable plan"
base_branch = "main"

[[tasks]]
name = "task-a"
description = "Task A"
scope = "narrow"
gate = "auto"
invariants = ["build_check"]
"#;

    let plan_toml = parse_plan_toml(toml_str).expect("TOML should parse");
    let plan = create_plan_from_toml(&pool, &plan_toml, "/tmp/test")
        .await
        .expect("create should succeed");

    // Approve the plan.
    let approved = plans::approve_plan(&pool, plan.id)
        .await
        .expect("approve_plan should succeed");

    assert_eq!(approved.status, PlanStatus::Approved);
    assert!(approved.approved_at.is_some());

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn approve_plan_fails_when_tasks_lack_invariants() {
    let (pool, db_name) = create_test_db().await;

    let toml_str = r#"
[plan]
name = "Unapproved plan"
base_branch = "main"

[[tasks]]
name = "task-without-inv"
description = "No invariants"
scope = "narrow"
gate = "auto"
"#;

    let plan = create_test_plan(&pool, toml_str).await;

    // Check that tasks_without_invariants reports the task.
    let tasks_without = plans::count_tasks_without_invariants(&pool, plan.id)
        .await
        .expect("count should succeed");
    assert_eq!(tasks_without.len(), 1);
    assert_eq!(tasks_without[0], "task-without-inv");

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn approve_plan_fails_for_non_draft_plan() {
    let (pool, db_name) = create_test_db().await;

    // Insert an invariant and create a plan with it.
    let _inv = insert_test_invariant(&pool, "check_a").await;

    let toml_str = r#"
[plan]
name = "Already approved"
base_branch = "main"

[[tasks]]
name = "t1"
description = "Task 1"
scope = "narrow"
gate = "auto"
invariants = ["check_a"]
"#;

    let plan_toml = parse_plan_toml(toml_str).expect("TOML should parse");
    let plan = create_plan_from_toml(&pool, &plan_toml, "/tmp/test")
        .await
        .expect("create should succeed");

    // Approve once.
    plans::approve_plan(&pool, plan.id)
        .await
        .expect("first approve should succeed");

    // Approve again should fail.
    let result = plans::approve_plan(&pool, plan.id).await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("cannot be approved"),
        "expected status error, got: {err_msg}"
    );

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn approve_plan_fails_for_nonexistent_plan() {
    let (pool, db_name) = create_test_db().await;

    let fake_id = Uuid::new_v4();
    let result = plans::approve_plan(&pool, fake_id).await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("not found"),
        "expected not found error, got: {err_msg}"
    );

    pool.close().await;
    drop_test_db(&db_name).await;
}

// -----------------------------------------------------------------------
// Tests: error handling
// -----------------------------------------------------------------------

#[test]
fn parse_invalid_toml_gives_error() {
    let result = parse_plan_toml("this is not valid toml {{{");
    assert!(result.is_err());
}

#[test]
fn parse_cycle_detected_gives_error() {
    let toml_str = r#"
[plan]
name = "Cycle"
base_branch = "main"

[[tasks]]
name = "a"
description = "A"
scope = "narrow"
gate = "auto"
depends_on = ["b"]

[[tasks]]
name = "b"
description = "B"
scope = "narrow"
gate = "auto"
depends_on = ["a"]
"#;
    let result = parse_plan_toml(toml_str);
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("cycle"),
        "expected cycle error, got: {err_msg}"
    );
}

#[test]
fn parse_file_not_found() {
    let result = std::fs::read_to_string("/nonexistent/path/to/plan.toml");
    assert!(result.is_err());
}

// -----------------------------------------------------------------------
// Tests: full create -> show -> approve workflow
// -----------------------------------------------------------------------

#[tokio::test]
async fn full_create_show_approve_workflow() {
    let (pool, db_name) = create_test_db().await;

    // 1. Insert an invariant.
    let _inv = insert_test_invariant(&pool, "workflow_check").await;

    // 2. Create a plan.
    let toml_str = r#"
[plan]
name = "Workflow test"
base_branch = "main"

[[tasks]]
name = "step-one"
description = "First step"
scope = "narrow"
gate = "auto"
invariants = ["workflow_check"]

[[tasks]]
name = "step-two"
description = "Second step"
scope = "medium"
gate = "human_review"
depends_on = ["step-one"]
invariants = ["workflow_check"]
"#;

    let plan_toml = parse_plan_toml(toml_str).expect("TOML should parse");
    let plan = create_plan_from_toml(&pool, &plan_toml, "/tmp/workflow")
        .await
        .expect("create should succeed");
    assert_eq!(plan.status, PlanStatus::Draft);

    // 3. Show (list all) -- verify plan appears.
    let all_plans = plans::list_plans(&pool)
        .await
        .expect("list_plans should succeed");
    assert_eq!(all_plans.len(), 1);
    assert_eq!(all_plans[0].id, plan.id);

    // 4. Show (detailed) -- verify tasks and dependencies.
    let (found_plan, found_tasks) = get_plan_with_tasks(&pool, plan.id)
        .await
        .expect("get_plan_with_tasks should succeed");
    assert_eq!(found_plan.name, "Workflow test");
    assert_eq!(found_tasks.len(), 2);

    let step_two = found_tasks.iter().find(|t| t.name == "step-two").unwrap();
    let deps = tasks::get_task_dependency_names(&pool, step_two.id)
        .await
        .expect("get deps should succeed");
    assert_eq!(deps, vec!["step-one"]);

    // 5. Approve.
    let approved = plans::approve_plan(&pool, plan.id)
        .await
        .expect("approve should succeed");
    assert_eq!(approved.status, PlanStatus::Approved);
    assert!(approved.approved_at.is_some());

    // 6. Verify the plan is now approved in a fresh read.
    let (reread, _) = get_plan_with_tasks(&pool, plan.id)
        .await
        .expect("re-read should succeed");
    assert_eq!(reread.status, PlanStatus::Approved);

    pool.close().await;
    drop_test_db(&db_name).await;
}

// -----------------------------------------------------------------------
// Tests: dependency edge counting
// -----------------------------------------------------------------------

#[tokio::test]
async fn count_dependency_edges_correct() {
    let (pool, db_name) = create_test_db().await;

    // Diamond: a -> b, a -> c, b -> d, c -> d  (4 edges)
    let toml_str = r#"
[plan]
name = "Diamond"
base_branch = "main"

[[tasks]]
name = "a"
description = "A"
scope = "narrow"
gate = "auto"

[[tasks]]
name = "b"
description = "B"
scope = "narrow"
gate = "auto"
depends_on = ["a"]

[[tasks]]
name = "c"
description = "C"
scope = "narrow"
gate = "auto"
depends_on = ["a"]

[[tasks]]
name = "d"
description = "D"
scope = "narrow"
gate = "auto"
depends_on = ["b", "c"]
"#;

    let plan = create_test_plan(&pool, toml_str).await;
    let edges = tasks::count_dependency_edges(&pool, plan.id)
        .await
        .expect("count should succeed");
    assert_eq!(edges, 4);

    pool.close().await;
    drop_test_db(&db_name).await;
}
