//! Integration tests for invariant CRUD operations.
//!
//! Each test creates a unique temporary database inside a shared containerized
//! PostgreSQL instance (via testcontainers), runs migrations, and drops it on
//! completion so tests are fully isolated and idempotent.

use uuid::Uuid;

use gator_db::models::{InvariantKind, InvariantScope};
use gator_db::queries::invariants::{self, NewInvariant};

use gator_test_utils::{create_test_db, drop_test_db};

/// Helper: build a NewInvariant with sensible defaults for testing.
fn test_new_invariant(name: &str) -> NewInvariant<'_> {
    NewInvariant {
        name,
        description: Some("test invariant"),
        kind: InvariantKind::Custom,
        command: "true",
        args: &[],
        expected_exit_code: 0,
        threshold: None,
        scope: InvariantScope::Project,
        timeout_secs: 300,
    }
}

// ---- Tests ----

#[tokio::test]
async fn insert_and_get_invariant() {
    let (pool, db_name) = create_test_db().await;

    let new = test_new_invariant("rust_build");
    let inserted = invariants::insert_invariant(&pool, &new)
        .await
        .expect("insert should succeed");

    assert_eq!(inserted.name, "rust_build");
    assert_eq!(inserted.description.as_deref(), Some("test invariant"));
    assert_eq!(inserted.kind, InvariantKind::Custom);
    assert_eq!(inserted.command, "true");
    assert!(inserted.args.is_empty());
    assert_eq!(inserted.expected_exit_code, 0);
    assert!(inserted.threshold.is_none());
    assert_eq!(inserted.scope, InvariantScope::Project);

    // Fetch by ID.
    let fetched = invariants::get_invariant(&pool, inserted.id)
        .await
        .expect("get should succeed")
        .expect("invariant should exist");
    assert_eq!(fetched.id, inserted.id);
    assert_eq!(fetched.name, "rust_build");

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn get_invariant_by_name() {
    let (pool, db_name) = create_test_db().await;

    let new = test_new_invariant("clippy_check");
    let inserted = invariants::insert_invariant(&pool, &new)
        .await
        .expect("insert should succeed");

    let fetched = invariants::get_invariant_by_name(&pool, "clippy_check")
        .await
        .expect("get_by_name should succeed")
        .expect("invariant should exist");
    assert_eq!(fetched.id, inserted.id);

    // Non-existent name returns None.
    let missing = invariants::get_invariant_by_name(&pool, "nonexistent")
        .await
        .expect("get_by_name should succeed");
    assert!(missing.is_none());

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn list_invariants_empty_and_populated() {
    let (pool, db_name) = create_test_db().await;

    // Initially empty.
    let list = invariants::list_invariants(&pool)
        .await
        .expect("list should succeed");
    assert!(list.is_empty());

    // Insert two invariants.
    let new_a = test_new_invariant("aaa_first");
    let new_b = test_new_invariant("zzz_last");
    invariants::insert_invariant(&pool, &new_a)
        .await
        .expect("insert a");
    invariants::insert_invariant(&pool, &new_b)
        .await
        .expect("insert b");

    let list = invariants::list_invariants(&pool)
        .await
        .expect("list should succeed");
    assert_eq!(list.len(), 2);
    // Ordered by name.
    assert_eq!(list[0].name, "aaa_first");
    assert_eq!(list[1].name, "zzz_last");

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn unique_name_constraint() {
    let (pool, db_name) = create_test_db().await;

    let new = test_new_invariant("unique_test");
    invariants::insert_invariant(&pool, &new)
        .await
        .expect("first insert should succeed");

    // Second insert with the same name should fail.
    let result = invariants::insert_invariant(&pool, &new).await;
    assert!(result.is_err(), "duplicate name should be rejected");

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn delete_unlinked_invariant() {
    let (pool, db_name) = create_test_db().await;

    let new = test_new_invariant("deletable");
    let inserted = invariants::insert_invariant(&pool, &new)
        .await
        .expect("insert should succeed");

    // Delete should succeed (not linked to any tasks).
    invariants::delete_invariant(&pool, inserted.id)
        .await
        .expect("delete should succeed");

    // Verify it's gone.
    let fetched = invariants::get_invariant(&pool, inserted.id)
        .await
        .expect("get should succeed");
    assert!(fetched.is_none(), "invariant should be deleted");

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn delete_nonexistent_invariant_fails() {
    let (pool, db_name) = create_test_db().await;

    let result = invariants::delete_invariant(&pool, Uuid::new_v4()).await;
    assert!(
        result.is_err(),
        "deleting nonexistent invariant should fail"
    );

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn cannot_delete_linked_invariant() {
    let (pool, db_name) = create_test_db().await;

    // Create an invariant.
    let new = test_new_invariant("linked_inv");
    let inv = invariants::insert_invariant(&pool, &new)
        .await
        .expect("insert invariant");

    // Create a plan and task to link the invariant to.
    let plan_id: (Uuid,) = sqlx::query_as(
        "INSERT INTO plans (name, project_path, base_branch) \
         VALUES ('test plan', '/tmp', 'main') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("insert plan");

    let task_id: (Uuid,) = sqlx::query_as(
        "INSERT INTO tasks (plan_id, name, description, scope_level, gate_policy) \
         VALUES ($1, 'test task', 'desc', 'narrow', 'auto') RETURNING id",
    )
    .bind(plan_id.0)
    .fetch_one(&pool)
    .await
    .expect("insert task");

    // Link the invariant to the task.
    invariants::link_task_invariant(&pool, task_id.0, inv.id)
        .await
        .expect("link should succeed");

    // Attempt to delete should fail.
    let result = invariants::delete_invariant(&pool, inv.id).await;
    assert!(
        result.is_err(),
        "should not be able to delete an invariant linked to tasks"
    );
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("linked to"),
        "error should mention the link, got: {err_msg}"
    );

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn get_invariants_for_task() {
    let (pool, db_name) = create_test_db().await;

    // Create two invariants.
    let new_a = test_new_invariant("inv_alpha");
    let new_b = test_new_invariant("inv_beta");
    let inv_a = invariants::insert_invariant(&pool, &new_a)
        .await
        .expect("insert a");
    let inv_b = invariants::insert_invariant(&pool, &new_b)
        .await
        .expect("insert b");

    // Create a plan and task.
    let plan_id: (Uuid,) = sqlx::query_as(
        "INSERT INTO plans (name, project_path, base_branch) \
         VALUES ('test plan', '/tmp', 'main') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("insert plan");

    let task_id: (Uuid,) = sqlx::query_as(
        "INSERT INTO tasks (plan_id, name, description, scope_level, gate_policy) \
         VALUES ($1, 'test task', 'desc', 'narrow', 'auto') RETURNING id",
    )
    .bind(plan_id.0)
    .fetch_one(&pool)
    .await
    .expect("insert task");

    // Link both invariants.
    invariants::link_task_invariant(&pool, task_id.0, inv_a.id)
        .await
        .expect("link a");
    invariants::link_task_invariant(&pool, task_id.0, inv_b.id)
        .await
        .expect("link b");

    // Fetch invariants for task.
    let linked = invariants::get_invariants_for_task(&pool, task_id.0)
        .await
        .expect("get_invariants_for_task should succeed");

    assert_eq!(linked.len(), 2);
    // Ordered by name.
    assert_eq!(linked[0].name, "inv_alpha");
    assert_eq!(linked[1].name, "inv_beta");

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn link_task_invariant_is_idempotent() {
    let (pool, db_name) = create_test_db().await;

    let new = test_new_invariant("idem_inv");
    let inv = invariants::insert_invariant(&pool, &new)
        .await
        .expect("insert invariant");

    let plan_id: (Uuid,) = sqlx::query_as(
        "INSERT INTO plans (name, project_path, base_branch) \
         VALUES ('test plan', '/tmp', 'main') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("insert plan");

    let task_id: (Uuid,) = sqlx::query_as(
        "INSERT INTO tasks (plan_id, name, description, scope_level, gate_policy) \
         VALUES ($1, 'test task', 'desc', 'narrow', 'auto') RETURNING id",
    )
    .bind(plan_id.0)
    .fetch_one(&pool)
    .await
    .expect("insert task");

    // Link twice -- second call should be a no-op.
    invariants::link_task_invariant(&pool, task_id.0, inv.id)
        .await
        .expect("first link");
    invariants::link_task_invariant(&pool, task_id.0, inv.id)
        .await
        .expect("second link (idempotent)");

    let linked = invariants::get_invariants_for_task(&pool, task_id.0)
        .await
        .expect("get_invariants_for_task");
    assert_eq!(linked.len(), 1, "should only be linked once");

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn insert_invariant_with_all_fields() {
    let (pool, db_name) = create_test_db().await;

    let args = vec!["--workspace".to_owned(), "--release".to_owned()];
    let new = NewInvariant {
        name: "full_test",
        description: Some("A fully-specified invariant"),
        kind: InvariantKind::Coverage,
        command: "cargo",
        args: &args,
        expected_exit_code: 0,
        threshold: Some(80.0),
        scope: InvariantScope::Global,
        timeout_secs: 300,
    };

    let inserted = invariants::insert_invariant(&pool, &new)
        .await
        .expect("insert should succeed");

    assert_eq!(inserted.name, "full_test");
    assert_eq!(
        inserted.description.as_deref(),
        Some("A fully-specified invariant")
    );
    assert_eq!(inserted.kind, InvariantKind::Coverage);
    assert_eq!(inserted.command, "cargo");
    assert_eq!(inserted.args, vec!["--workspace", "--release"]);
    assert_eq!(inserted.expected_exit_code, 0);
    assert_eq!(inserted.threshold, Some(80.0));
    assert_eq!(inserted.scope, InvariantScope::Global);

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn get_invariant_nonexistent_returns_none() {
    let (pool, db_name) = create_test_db().await;

    let result = invariants::get_invariant(&pool, Uuid::new_v4())
        .await
        .expect("get should succeed");
    assert!(result.is_none());

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn get_invariants_for_task_with_no_links() {
    let (pool, db_name) = create_test_db().await;

    let plan_id: (Uuid,) = sqlx::query_as(
        "INSERT INTO plans (name, project_path, base_branch) \
         VALUES ('test plan', '/tmp', 'main') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("insert plan");

    let task_id: (Uuid,) = sqlx::query_as(
        "INSERT INTO tasks (plan_id, name, description, scope_level, gate_policy) \
         VALUES ($1, 'test task', 'desc', 'narrow', 'auto') RETURNING id",
    )
    .bind(plan_id.0)
    .fetch_one(&pool)
    .await
    .expect("insert task");

    let linked = invariants::get_invariants_for_task(&pool, task_id.0)
        .await
        .expect("should succeed");
    assert!(linked.is_empty());

    pool.close().await;
    drop_test_db(&db_name).await;
}
