//! Convenience dispatch helpers that wrap [`super::TaskStateMachine`]
//! transitions with semantic names.

use std::path::Path;

use anyhow::Result;
use sqlx::PgPool;
use uuid::Uuid;

use gator_db::models::TaskStatus;

use super::TaskStateMachine;

/// Assign a task to a harness and worktree.
///
/// Validates that all dependencies are `passed`, sets metadata,
/// and transitions `pending -> assigned`.
pub async fn assign_task(
    pool: &PgPool,
    task_id: Uuid,
    harness: &str,
    worktree_path: &Path,
) -> Result<()> {
    TaskStateMachine::assign_task(pool, task_id, harness, worktree_path).await
}

/// Start a task: transition `assigned -> running`.
///
/// Sets `started_at` to the current timestamp.
pub async fn start_task(pool: &PgPool, task_id: Uuid) -> Result<()> {
    TaskStateMachine::transition(pool, task_id, TaskStatus::Assigned, TaskStatus::Running).await
}

/// Begin checking a task's invariants: transition `running -> checking`.
pub async fn begin_checking(pool: &PgPool, task_id: Uuid) -> Result<()> {
    TaskStateMachine::transition(pool, task_id, TaskStatus::Running, TaskStatus::Checking).await
}

/// Mark a task as passed: transition `checking -> passed`.
///
/// Sets `completed_at` to the current timestamp.
pub async fn pass_task(pool: &PgPool, task_id: Uuid) -> Result<()> {
    TaskStateMachine::transition(pool, task_id, TaskStatus::Checking, TaskStatus::Passed).await
}

/// Mark a task as failed: transition `checking -> failed`.
///
/// Sets `completed_at` to the current timestamp.
pub async fn fail_task(pool: &PgPool, task_id: Uuid) -> Result<()> {
    TaskStateMachine::transition(pool, task_id, TaskStatus::Checking, TaskStatus::Failed).await
}

/// Retry a failed task: transition `failed -> assigned`.
///
/// Increments the attempt counter. Fails if `attempt >= retry_max`.
pub async fn retry_task(pool: &PgPool, task_id: Uuid) -> Result<()> {
    TaskStateMachine::transition(pool, task_id, TaskStatus::Failed, TaskStatus::Assigned).await
}

/// Escalate a failed task: transition `failed -> escalated`.
///
/// Sets `completed_at` to the current timestamp.
pub async fn escalate_task(pool: &PgPool, task_id: Uuid) -> Result<()> {
    TaskStateMachine::transition(pool, task_id, TaskStatus::Failed, TaskStatus::Escalated).await
}
