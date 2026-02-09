//! Convenience dispatch helpers that wrap [`super::TaskStateMachine`]
//! transitions with semantic names.

use std::path::Path;

use anyhow::{Context, Result, bail};
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

/// Operator approval: transition a `checking` task to `passed`.
///
/// This is the operator path for tasks awaiting human review/approval.
/// The task must be in `checking` status.
pub async fn approve_task(pool: &PgPool, task_id: Uuid) -> Result<()> {
    let task = gator_db::queries::tasks::get_task(pool, task_id)
        .await?
        .with_context(|| format!("task {task_id} not found"))?;

    if task.status != TaskStatus::Checking {
        bail!(
            "task {} is {}, must be checking to approve",
            task_id,
            task.status
        );
    }

    TaskStateMachine::transition(pool, task_id, TaskStatus::Checking, TaskStatus::Passed).await
}

/// Operator rejection: transition a `checking` task to `failed`.
///
/// The task can then be retried or escalated.
pub async fn reject_task(pool: &PgPool, task_id: Uuid) -> Result<()> {
    let task = gator_db::queries::tasks::get_task(pool, task_id)
        .await?
        .with_context(|| format!("task {task_id} not found"))?;

    if task.status != TaskStatus::Checking {
        bail!(
            "task {} is {}, must be checking to reject",
            task_id,
            task.status
        );
    }

    TaskStateMachine::transition(pool, task_id, TaskStatus::Checking, TaskStatus::Failed).await
}

/// Operator retry: reset a failed or escalated task back to pending.
///
/// For `failed` tasks: respects retry_max unless `force` is true.
/// For `escalated` tasks: always allowed (operator override).
pub async fn operator_retry_task(pool: &PgPool, task_id: Uuid, force: bool) -> Result<()> {
    let task = gator_db::queries::tasks::get_task(pool, task_id)
        .await?
        .with_context(|| format!("task {task_id} not found"))?;

    match task.status {
        TaskStatus::Failed => {
            if !force && task.attempt >= task.retry_max {
                bail!(
                    "task {} has exhausted retries (attempt {}/{}); use --force to override",
                    task_id,
                    task.attempt,
                    task.retry_max
                );
            }
            let rows = gator_db::queries::tasks::retry_task_to_pending(pool, task_id, task.attempt)
                .await?;
            if rows == 0 {
                bail!("optimistic lock failed on retry for task {}", task_id);
            }
        }
        TaskStatus::Escalated => {
            let rows =
                gator_db::queries::tasks::retry_escalated_to_pending(pool, task_id, task.attempt)
                    .await?;
            if rows == 0 {
                bail!(
                    "optimistic lock failed on retry-from-escalated for task {}",
                    task_id
                );
            }
        }
        _ => {
            bail!(
                "task {} is {}, must be failed or escalated to retry",
                task_id,
                task.status
            );
        }
    }

    Ok(())
}
