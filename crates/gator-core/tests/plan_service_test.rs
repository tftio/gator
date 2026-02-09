//! Integration tests for the plan service layer.
//!
//! Tests `create_plan_from_toml` and `get_plan_with_tasks` against a real
//! PostgreSQL database. Each test creates an isolated temporary database.

use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgPool};
use uuid::Uuid;

use gator_core::plan::{
    create_plan_from_toml, get_plan_with_tasks, materialize_plan, materialize_task, parse_plan_toml,
};
use gator_db::config::DbConfig;
use gator_db::models::PlanStatus;
use gator_db::pool;
use gator_db::queries::tasks;

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

#[tokio::test]
async fn create_plan_from_toml_inserts_plan_and_tasks() {
    let (pool, db_name) = create_temp_db().await;

    let toml_str = r#"
[plan]
name = "Integration test plan"
base_branch = "main"

[[tasks]]
name = "task-a"
description = "First task"
scope = "narrow"
gate = "auto"

[[tasks]]
name = "task-b"
description = "Second task, depends on A"
scope = "medium"
gate = "human_review"
depends_on = ["task-a"]
"#;
    let plan_toml = parse_plan_toml(toml_str).expect("should parse");

    let (plan, warnings) = create_plan_from_toml(&pool, &plan_toml, "/tmp/project")
        .await
        .expect("create_plan_from_toml should succeed");

    // No invariants referenced, so no warnings about missing invariants.
    assert!(warnings.is_empty(), "warnings: {warnings:?}");

    assert_eq!(plan.name, "Integration test plan");
    assert_eq!(plan.base_branch, "main");
    assert_eq!(plan.status, PlanStatus::Draft);

    // Verify tasks were created.
    let task_list = tasks::list_tasks_for_plan(&pool, plan.id).await.unwrap();
    assert_eq!(task_list.len(), 2);

    let task_a = task_list.iter().find(|t| t.name == "task-a").unwrap();
    let task_b = task_list.iter().find(|t| t.name == "task-b").unwrap();

    assert_eq!(task_a.description, "First task");
    assert_eq!(task_b.description, "Second task, depends on A");

    // Verify dependency: task-b depends on task-a.
    let b_deps = tasks::get_task_dependencies(&pool, task_b.id)
        .await
        .unwrap();
    assert_eq!(b_deps, vec![task_a.id]);

    let a_deps = tasks::get_task_dependencies(&pool, task_a.id)
        .await
        .unwrap();
    assert!(a_deps.is_empty());

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn create_plan_warns_on_unknown_invariants() {
    let (pool, db_name) = create_temp_db().await;

    let toml_str = r#"
[plan]
name = "Invariant warning test"
base_branch = "main"

[[tasks]]
name = "t"
description = "test"
scope = "narrow"
gate = "auto"
invariants = ["nonexistent_inv"]
"#;
    let plan_toml = parse_plan_toml(toml_str).expect("should parse");

    let (plan, warnings) = create_plan_from_toml(&pool, &plan_toml, "/tmp")
        .await
        .expect("should succeed despite unknown invariant");

    assert_eq!(plan.name, "Invariant warning test");
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("nonexistent_inv"));

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn create_plan_links_existing_invariants() {
    let (pool, db_name) = create_temp_db().await;

    // Insert an invariant first.
    sqlx::query(
        "INSERT INTO invariants (name, kind, command) VALUES ('my_check', 'custom', 'true')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let toml_str = r#"
[plan]
name = "Invariant link test"
base_branch = "main"

[[tasks]]
name = "t"
description = "task with invariant"
scope = "narrow"
gate = "auto"
invariants = ["my_check", "missing_one"]
"#;
    let plan_toml = parse_plan_toml(toml_str).expect("should parse");

    let (plan, warnings) = create_plan_from_toml(&pool, &plan_toml, "/tmp")
        .await
        .expect("should succeed");

    // One invariant linked, one warning.
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("missing_one"));

    // Verify the link was created.
    let task_list = tasks::list_tasks_for_plan(&pool, plan.id).await.unwrap();
    let task = &task_list[0];

    let linked: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM task_invariants WHERE task_id = $1")
        .bind(task.id)
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(linked.0, 1);

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn get_plan_with_tasks_returns_complete_data() {
    let (pool, db_name) = create_temp_db().await;

    let toml_str = r#"
[plan]
name = "Get plan test"
base_branch = "develop"

[[tasks]]
name = "alpha"
description = "Alpha task"
scope = "broad"
gate = "human_approve"
retry_max = 5

[[tasks]]
name = "beta"
description = "Beta task"
scope = "narrow"
gate = "auto"
depends_on = ["alpha"]
"#;
    let plan_toml = parse_plan_toml(toml_str).expect("should parse");
    let (plan, _) = create_plan_from_toml(&pool, &plan_toml, "/tmp/proj")
        .await
        .expect("create should succeed");

    let (fetched_plan, fetched_tasks) = get_plan_with_tasks(&pool, plan.id)
        .await
        .expect("get_plan_with_tasks should succeed");

    assert_eq!(fetched_plan.id, plan.id);
    assert_eq!(fetched_plan.name, "Get plan test");
    assert_eq!(fetched_plan.base_branch, "develop");
    assert_eq!(fetched_tasks.len(), 2);

    let alpha = fetched_tasks.iter().find(|t| t.name == "alpha").unwrap();
    assert_eq!(alpha.retry_max, 5);

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn get_plan_with_tasks_fails_for_missing_plan() {
    let (pool, db_name) = create_temp_db().await;

    let result = get_plan_with_tasks(&pool, Uuid::new_v4()).await;
    assert!(result.is_err());

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn create_plan_with_diamond_dependencies() {
    let (pool, db_name) = create_temp_db().await;

    let toml_str = r#"
[plan]
name = "Diamond"
base_branch = "main"

[[tasks]]
name = "root"
description = "Root task"
scope = "narrow"
gate = "auto"

[[tasks]]
name = "left"
description = "Left branch"
scope = "narrow"
gate = "auto"
depends_on = ["root"]

[[tasks]]
name = "right"
description = "Right branch"
scope = "narrow"
gate = "auto"
depends_on = ["root"]

[[tasks]]
name = "merge"
description = "Merge point"
scope = "medium"
gate = "human_review"
depends_on = ["left", "right"]
"#;
    let plan_toml = parse_plan_toml(toml_str).expect("should parse");
    let (plan, warnings) = create_plan_from_toml(&pool, &plan_toml, "/tmp")
        .await
        .expect("should succeed");

    assert!(warnings.is_empty());

    let task_list = tasks::list_tasks_for_plan(&pool, plan.id).await.unwrap();
    assert_eq!(task_list.len(), 4);

    let merge = task_list.iter().find(|t| t.name == "merge").unwrap();
    let mut merge_deps = tasks::get_task_dependencies(&pool, merge.id).await.unwrap();
    merge_deps.sort();

    let left_id = task_list.iter().find(|t| t.name == "left").unwrap().id;
    let right_id = task_list.iter().find(|t| t.name == "right").unwrap().id;
    let mut expected = vec![left_id, right_id];
    expected.sort();

    assert_eq!(merge_deps, expected);

    pool.close().await;
    drop_temp_db(&db_name).await;
}

// -----------------------------------------------------------------------
// Round-trip test: parse -> DB -> materialize -> parse
// -----------------------------------------------------------------------

#[tokio::test]
async fn roundtrip_parse_db_materialize_parse() {
    let (pool, db_name) = create_temp_db().await;

    let original_toml = r#"
[plan]
name = "Round-trip test"
base_branch = "develop"

[[tasks]]
name = "task-alpha"
description = "First task in the chain"
scope = "narrow"
gate = "auto"
retry_max = 2

[[tasks]]
name = "task-beta"
description = "Second task, depends on alpha"
scope = "medium"
gate = "human_review"
retry_max = 5
depends_on = ["task-alpha"]

[[tasks]]
name = "task-gamma"
description = "Third task, depends on both"
scope = "broad"
gate = "human_approve"
depends_on = ["task-alpha", "task-beta"]
"#;

    // Step 1: Parse the original TOML.
    let original_plan = parse_plan_toml(original_toml).expect("should parse original");

    // Step 2: Insert into DB.
    let (plan, warnings) = create_plan_from_toml(&pool, &original_plan, "/tmp/roundtrip")
        .await
        .expect("create should succeed");
    assert!(warnings.is_empty());

    // Step 3: Materialize from DB.
    let materialized_toml = materialize_plan(&pool, plan.id)
        .await
        .expect("materialize should succeed");

    // Step 4: Parse the materialized TOML.
    // The materialized TOML has an extra `status` field per task, but TaskToml
    // ignores unknown fields.
    let reparsed: gator_core::plan::PlanToml =
        toml::from_str(&materialized_toml).expect("should parse materialized TOML");

    // Verify plan metadata matches.
    assert_eq!(reparsed.plan.name, original_plan.plan.name);
    assert_eq!(reparsed.plan.base_branch, original_plan.plan.base_branch);

    // Verify tasks match.
    assert_eq!(reparsed.tasks.len(), original_plan.tasks.len());

    for original_task in &original_plan.tasks {
        let reparsed_task = reparsed
            .tasks
            .iter()
            .find(|t| t.name == original_task.name)
            .unwrap_or_else(|| {
                panic!(
                    "task {:?} not found in materialized output",
                    original_task.name
                )
            });

        assert_eq!(
            reparsed_task.description, original_task.description,
            "description mismatch for task {:?}",
            original_task.name
        );
        assert_eq!(
            reparsed_task.scope, original_task.scope,
            "scope mismatch for task {:?}",
            original_task.name
        );
        assert_eq!(
            reparsed_task.gate, original_task.gate,
            "gate mismatch for task {:?}",
            original_task.name
        );
        assert_eq!(
            reparsed_task.retry_max, original_task.retry_max,
            "retry_max mismatch for task {:?}",
            original_task.name
        );

        // Verify dependencies match (order may differ).
        let mut original_deps = original_task.depends_on.clone();
        original_deps.sort();
        let mut reparsed_deps = reparsed_task.depends_on.clone();
        reparsed_deps.sort();
        assert_eq!(
            reparsed_deps, original_deps,
            "dependency mismatch for task {:?}",
            original_task.name
        );

        // Invariants: original had none, reparsed should have none.
        assert_eq!(
            reparsed_task.invariants, original_task.invariants,
            "invariant mismatch for task {:?}",
            original_task.name
        );
    }

    // Verify that `status` is present in the raw TOML text.
    assert!(
        materialized_toml.contains("status = \"pending\""),
        "materialized TOML should include task status"
    );

    pool.close().await;
    drop_temp_db(&db_name).await;
}

// -----------------------------------------------------------------------
// Round-trip test with invariants
// -----------------------------------------------------------------------

#[tokio::test]
async fn roundtrip_with_invariants() {
    let (pool, db_name) = create_temp_db().await;

    // Insert invariants into the DB first.
    sqlx::query(
        "INSERT INTO invariants (name, kind, command) VALUES ('rust_build', 'custom', 'cargo')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO invariants (name, kind, command) VALUES ('rust_test', 'test_suite', 'cargo')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let original_toml = r#"
[plan]
name = "Invariant round-trip"
base_branch = "main"

[[tasks]]
name = "build-it"
description = "Build the project"
scope = "narrow"
gate = "auto"
invariants = ["rust_build", "rust_test"]

[[tasks]]
name = "deploy-it"
description = "Deploy the build"
scope = "medium"
gate = "human_review"
depends_on = ["build-it"]
invariants = ["rust_build"]
"#;

    let original_plan = parse_plan_toml(original_toml).expect("should parse");
    let (plan, warnings) = create_plan_from_toml(&pool, &original_plan, "/tmp/inv-roundtrip")
        .await
        .expect("create should succeed");
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

    // Materialize.
    let materialized = materialize_plan(&pool, plan.id)
        .await
        .expect("materialize should succeed");

    // Re-parse.
    let reparsed: gator_core::plan::PlanToml =
        toml::from_str(&materialized).expect("should parse materialized TOML");

    // Check invariant links.
    let build_task = reparsed
        .tasks
        .iter()
        .find(|t| t.name == "build-it")
        .unwrap();
    let mut build_invs = build_task.invariants.clone();
    build_invs.sort();
    assert_eq!(build_invs, vec!["rust_build", "rust_test"]);

    let deploy_task = reparsed
        .tasks
        .iter()
        .find(|t| t.name == "deploy-it")
        .unwrap();
    assert_eq!(deploy_task.invariants, vec!["rust_build"]);

    pool.close().await;
    drop_temp_db(&db_name).await;
}

// -----------------------------------------------------------------------
// materialize_task produces clean markdown
// -----------------------------------------------------------------------

#[tokio::test]
async fn materialize_task_produces_clean_markdown() {
    let (pool, db_name) = create_temp_db().await;

    // Insert invariants.
    sqlx::query(
        "INSERT INTO invariants (name, description, kind, command, args) \
         VALUES ('cargo_build', 'Compile the project', 'custom', 'cargo', '{build,--workspace}')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let toml_str = r#"
[plan]
name = "Task materialize test"
base_branch = "main"

[[tasks]]
name = "root-task"
description = "This is the root task.\nIt has multiple lines."
scope = "narrow"
gate = "auto"
invariants = ["cargo_build"]

[[tasks]]
name = "child-task"
description = "Depends on root"
scope = "medium"
gate = "human_review"
depends_on = ["root-task"]
"#;

    let plan_toml = parse_plan_toml(toml_str).expect("should parse");
    let (plan, _) = create_plan_from_toml(&pool, &plan_toml, "/tmp/task-mat")
        .await
        .expect("create should succeed");

    let task_list = tasks::list_tasks_for_plan(&pool, plan.id).await.unwrap();

    // Materialize the root task.
    let root = task_list.iter().find(|t| t.name == "root-task").unwrap();
    let root_md = materialize_task(&pool, root.id)
        .await
        .expect("materialize_task should succeed");

    // Verify markdown structure.
    assert!(root_md.contains("# Task: root-task"), "should have title");
    assert!(
        root_md.contains("**Status:** pending"),
        "should have status"
    );
    assert!(root_md.contains("**Scope:** narrow"), "should have scope");
    assert!(
        root_md.contains("**Gate policy:** auto"),
        "should have gate"
    );
    assert!(
        root_md.contains("## Description"),
        "should have description section"
    );
    assert!(
        root_md.contains("This is the root task."),
        "should contain description text"
    );
    assert!(
        root_md.contains("## Invariants"),
        "should have invariants section"
    );
    assert!(
        root_md.contains("cargo_build"),
        "should list invariant name"
    );
    assert!(
        root_md.contains("`cargo build --workspace`"),
        "should show invariant command"
    );
    // Root task has no dependencies, so no Dependencies section.
    assert!(
        !root_md.contains("## Dependencies"),
        "root task should not have dependencies section"
    );

    // Materialize the child task.
    let child = task_list.iter().find(|t| t.name == "child-task").unwrap();
    let child_md = materialize_task(&pool, child.id)
        .await
        .expect("materialize_task should succeed");

    assert!(child_md.contains("# Task: child-task"), "should have title");
    assert!(
        child_md.contains("## Dependencies"),
        "child should have dependencies section"
    );
    assert!(
        child_md.contains("**root-task**: pending"),
        "should show dependency with status"
    );
    // Child has no invariants.
    assert!(
        !child_md.contains("## Invariants"),
        "child should not have invariants section"
    );

    // Verify no DB identifiers leak into the markdown.
    assert!(
        !child_md.contains(&plan.id.to_string()),
        "should not contain plan UUID"
    );
    assert!(
        !child_md.contains(&root.id.to_string()),
        "should not contain task UUID"
    );

    pool.close().await;
    drop_temp_db(&db_name).await;
}
