//! Database query functions for the `tasks`, `task_dependencies`, and
//! `task_invariants` tables.

use anyhow::{Context, Result};
use sqlx::PgPool;
use uuid::Uuid;

use crate::models::{Task, TaskStatus};

/// Insert a new task row. Returns the inserted task with server-generated
/// defaults (id, created_at, status, attempt).
///
/// `scope_level` and `gate_policy` are passed as strings that must match the
/// CHECK constraints on the `tasks` table (e.g. "narrow", "auto").
#[allow(clippy::too_many_arguments)]
pub async fn insert_task(
    pool: &PgPool,
    plan_id: Uuid,
    name: &str,
    description: &str,
    scope_level: &str,
    gate_policy: &str,
    retry_max: i32,
    requested_harness: Option<&str>,
) -> Result<Task> {
    let task = sqlx::query_as::<_, Task>(
        "INSERT INTO tasks (plan_id, name, description, scope_level, gate_policy, retry_max, requested_harness) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         RETURNING *",
    )
    .bind(plan_id)
    .bind(name)
    .bind(description)
    .bind(scope_level)
    .bind(gate_policy)
    .bind(retry_max)
    .bind(requested_harness)
    .fetch_one(pool)
    .await
    .context("failed to insert task")?;

    Ok(task)
}

/// Fetch a single task by ID.
pub async fn get_task(pool: &PgPool, id: Uuid) -> Result<Option<Task>> {
    let task = sqlx::query_as::<_, Task>("SELECT * FROM tasks WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .context("failed to fetch task")?;

    Ok(task)
}

/// List all tasks for a given plan, ordered by creation time.
pub async fn list_tasks_for_plan(pool: &PgPool, plan_id: Uuid) -> Result<Vec<Task>> {
    let tasks =
        sqlx::query_as::<_, Task>("SELECT * FROM tasks WHERE plan_id = $1 ORDER BY created_at ASC")
            .bind(plan_id)
            .fetch_all(pool)
            .await
            .context("failed to list tasks for plan")?;

    Ok(tasks)
}

/// Update the status of a task.
pub async fn update_task_status(pool: &PgPool, id: Uuid, status: TaskStatus) -> Result<()> {
    let result = sqlx::query("UPDATE tasks SET status = $1 WHERE id = $2")
        .bind(status)
        .bind(id)
        .execute(pool)
        .await
        .context("failed to update task status")?;

    if result.rows_affected() == 0 {
        anyhow::bail!("task {id} not found");
    }

    Ok(())
}

/// Insert a dependency edge: `task_id` depends on `depends_on_id`.
///
/// Uses `ON CONFLICT DO NOTHING` so this is idempotent.
pub async fn insert_task_dependency(
    pool: &PgPool,
    task_id: Uuid,
    depends_on_id: Uuid,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO task_dependencies (task_id, depends_on) VALUES ($1, $2) \
         ON CONFLICT DO NOTHING",
    )
    .bind(task_id)
    .bind(depends_on_id)
    .execute(pool)
    .await
    .context("failed to insert task dependency")?;

    Ok(())
}

/// Get the IDs of all tasks that a given task depends on.
pub async fn get_task_dependencies(pool: &PgPool, task_id: Uuid) -> Result<Vec<Uuid>> {
    let rows: Vec<(Uuid,)> =
        sqlx::query_as("SELECT depends_on FROM task_dependencies WHERE task_id = $1")
            .bind(task_id)
            .fetch_all(pool)
            .await
            .context("failed to get task dependencies")?;

    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Get the names of all tasks that a given task depends on (resolving through
/// the tasks table).
pub async fn get_task_dependency_names(pool: &PgPool, task_id: Uuid) -> Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT dep.name FROM task_dependencies td \
         JOIN tasks dep ON dep.id = td.depends_on \
         WHERE td.task_id = $1 \
         ORDER BY dep.name",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await
    .context("failed to get task dependency names")?;

    Ok(rows.into_iter().map(|(name,)| name).collect())
}

/// Count total dependency edges for a plan.
pub async fn count_dependency_edges(pool: &PgPool, plan_id: Uuid) -> Result<i64> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM task_dependencies td \
         JOIN tasks t ON t.id = td.task_id \
         WHERE t.plan_id = $1",
    )
    .bind(plan_id)
    .fetch_one(pool)
    .await
    .context("failed to count dependency edges")?;

    Ok(row.0)
}

/// Link a task to an invariant.
///
/// Uses `ON CONFLICT DO NOTHING` so this is idempotent.
pub async fn link_task_invariant(pool: &PgPool, task_id: Uuid, invariant_id: Uuid) -> Result<()> {
    sqlx::query(
        "INSERT INTO task_invariants (task_id, invariant_id) VALUES ($1, $2) \
         ON CONFLICT DO NOTHING",
    )
    .bind(task_id)
    .bind(invariant_id)
    .execute(pool)
    .await
    .context("failed to link task to invariant")?;

    Ok(())
}

// -----------------------------------------------------------------------
// State-machine queries (T014)
// -----------------------------------------------------------------------

/// Atomically transition a task from one status to another.
///
/// Uses optimistic locking: the UPDATE's WHERE clause includes
/// `status = $from`, so the row is only updated if the current status
/// matches the expected `from` value. Returns the number of rows
/// affected (0 means the status did not match).
pub async fn transition_task_status(
    pool: &PgPool,
    task_id: Uuid,
    from: TaskStatus,
    to: TaskStatus,
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    completed_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<u64> {
    let result = sqlx::query(
        "UPDATE tasks \
         SET status = $1, \
             started_at = COALESCE($2, started_at), \
             completed_at = COALESCE($3, completed_at) \
         WHERE id = $4 AND status = $5",
    )
    .bind(to)
    .bind(started_at)
    .bind(completed_at)
    .bind(task_id)
    .bind(from)
    .execute(pool)
    .await
    .context("failed to transition task status")?;

    Ok(result.rows_affected())
}

/// Atomically transition a task from `failed` to `assigned` (retry),
/// incrementing the attempt counter and clearing timestamps. Uses
/// optimistic locking on both status and the current attempt value.
pub async fn transition_task_retry(
    pool: &PgPool,
    task_id: Uuid,
    current_attempt: i32,
) -> Result<u64> {
    let result = sqlx::query(
        "UPDATE tasks \
         SET status = 'assigned', \
             attempt = attempt + 1, \
             started_at = NULL, \
             completed_at = NULL \
         WHERE id = $1 AND status = 'failed' AND attempt = $2",
    )
    .bind(task_id)
    .bind(current_attempt)
    .execute(pool)
    .await
    .context("failed to retry task")?;

    Ok(result.rows_affected())
}

/// Set the assigned harness and worktree path on a task.
pub async fn assign_task_metadata(
    pool: &PgPool,
    task_id: Uuid,
    harness: &str,
    worktree_path: &str,
) -> Result<u64> {
    let result = sqlx::query(
        "UPDATE tasks \
         SET assigned_harness = $1, worktree_path = $2 \
         WHERE id = $3",
    )
    .bind(harness)
    .bind(worktree_path)
    .bind(task_id)
    .execute(pool)
    .await
    .context("failed to assign task metadata")?;

    Ok(result.rows_affected())
}

/// Get all tasks in a plan whose dependencies are all in `passed` status
/// and whose own status is `pending` (i.e. ready to be assigned).
pub async fn get_ready_tasks(pool: &PgPool, plan_id: Uuid) -> Result<Vec<Task>> {
    let tasks = sqlx::query_as::<_, Task>(
        "SELECT t.* \
         FROM tasks t \
         WHERE t.plan_id = $1 \
           AND t.status = 'pending' \
           AND NOT EXISTS ( \
               SELECT 1 FROM task_dependencies td \
               JOIN tasks dep ON dep.id = td.depends_on \
               WHERE td.task_id = t.id AND dep.status != 'passed' \
           )",
    )
    .bind(plan_id)
    .fetch_all(pool)
    .await
    .context("failed to get ready tasks")?;

    Ok(tasks)
}

/// Status counts for a plan's tasks.
#[derive(Debug, Clone, Default)]
pub struct PlanProgress {
    pub pending: i64,
    pub assigned: i64,
    pub running: i64,
    pub checking: i64,
    pub passed: i64,
    pub failed: i64,
    pub escalated: i64,
    pub total: i64,
}

/// Get a summary of task counts by status for a given plan.
pub async fn get_plan_progress(pool: &PgPool, plan_id: Uuid) -> Result<PlanProgress> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT status::text, COUNT(*) as cnt \
         FROM tasks \
         WHERE plan_id = $1 \
         GROUP BY status",
    )
    .bind(plan_id)
    .fetch_all(pool)
    .await
    .context("failed to get plan progress")?;

    let mut progress = PlanProgress::default();
    for (status, count) in &rows {
        match status.as_str() {
            "pending" => progress.pending = *count,
            "assigned" => progress.assigned = *count,
            "running" => progress.running = *count,
            "checking" => progress.checking = *count,
            "passed" => progress.passed = *count,
            "failed" => progress.failed = *count,
            "escalated" => progress.escalated = *count,
            _ => {}
        }
        progress.total += count;
    }
    Ok(progress)
}

/// Check whether all tasks in a plan have status `passed`.
pub async fn is_plan_complete(pool: &PgPool, plan_id: Uuid) -> Result<bool> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM tasks \
         WHERE plan_id = $1 AND status != 'passed'",
    )
    .bind(plan_id)
    .fetch_one(pool)
    .await
    .context("failed to check plan completion")?;

    Ok(row.0 == 0)
}

/// Reset tasks stuck in intermediate states (assigned, running, checking)
/// back to `failed` so they can be retried or escalated.
///
/// This is used for restart recovery: if the orchestrator crashes mid-run,
/// tasks that were in progress are left in limbo. This function resets them
/// so the orchestrator can decide whether to retry or escalate.
///
/// Returns the tasks that were reset.
pub async fn reset_orphaned_tasks(pool: &PgPool, plan_id: Uuid) -> Result<Vec<Task>> {
    let tasks = sqlx::query_as::<_, Task>(
        "UPDATE tasks \
         SET status = 'failed', \
             completed_at = NOW() \
         WHERE plan_id = $1 \
           AND status IN ('assigned', 'running', 'checking') \
         RETURNING *",
    )
    .bind(plan_id)
    .fetch_all(pool)
    .await
    .context("failed to reset orphaned tasks")?;

    Ok(tasks)
}

/// Reset an escalated task back to `pending` with an incremented attempt counter.
///
/// This is the operator override path: escalated tasks have exhausted their
/// normal retry budget, but the operator can force a retry.
pub async fn retry_escalated_to_pending(
    pool: &PgPool,
    task_id: Uuid,
    current_attempt: i32,
) -> Result<u64> {
    let result = sqlx::query(
        "UPDATE tasks \
         SET status = 'pending', \
             attempt = attempt + 1, \
             assigned_harness = NULL, \
             worktree_path = NULL, \
             started_at = NULL, \
             completed_at = NULL \
         WHERE id = $1 AND status = 'escalated' AND attempt = $2",
    )
    .bind(task_id)
    .bind(current_attempt)
    .execute(pool)
    .await
    .context("failed to retry escalated task to pending")?;

    Ok(result.rows_affected())
}

/// A task with its plan name (for cross-plan views like the review queue).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TaskWithPlanName {
    // Task fields
    pub id: uuid::Uuid,
    pub plan_id: uuid::Uuid,
    pub name: String,
    pub description: String,
    pub scope_level: crate::models::ScopeLevel,
    pub gate_policy: crate::models::GatePolicy,
    pub retry_max: i32,
    pub status: TaskStatus,
    pub assigned_harness: Option<String>,
    pub requested_harness: Option<String>,
    pub worktree_path: Option<String>,
    pub attempt: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    // Extra
    pub plan_name: String,
}

/// List all tasks in `checking` status across all plans.
pub async fn list_checking_tasks(pool: &PgPool) -> Result<Vec<TaskWithPlanName>> {
    let tasks = sqlx::query_as::<_, TaskWithPlanName>(
        "SELECT t.id, t.plan_id, t.name, t.description, t.scope_level, t.gate_policy, \
                t.retry_max, t.status, t.assigned_harness, t.requested_harness, \
                t.worktree_path, t.attempt, \
                t.created_at, t.started_at, t.completed_at, \
                p.name AS plan_name \
         FROM tasks t \
         JOIN plans p ON p.id = t.plan_id \
         WHERE t.status = 'checking' \
         ORDER BY t.created_at ASC",
    )
    .fetch_all(pool)
    .await
    .context("failed to list checking tasks")?;

    Ok(tasks)
}

/// Reset a failed task back to `pending` with an incremented attempt counter.
///
/// Unlike `transition_task_retry` (which sets status to `assigned`), this
/// resets to `pending` so the orchestrator's DAG scheduler can pick it up
/// through the normal `get_ready_tasks` path.
pub async fn retry_task_to_pending(
    pool: &PgPool,
    task_id: Uuid,
    current_attempt: i32,
) -> Result<u64> {
    let result = sqlx::query(
        "UPDATE tasks \
         SET status = 'pending', \
             attempt = attempt + 1, \
             assigned_harness = NULL, \
             worktree_path = NULL, \
             started_at = NULL, \
             completed_at = NULL \
         WHERE id = $1 AND status = 'failed' AND attempt = $2",
    )
    .bind(task_id)
    .bind(current_attempt)
    .execute(pool)
    .await
    .context("failed to retry task to pending")?;

    Ok(result.rows_affected())
}
