//! Task state machine transitions.
//!
//! Validates and executes state transitions for tasks, enforcing the
//! allowed transition graph, optimistic locking, timestamp management,
//! and retry limits.

pub mod dispatch;
pub mod queries;

use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use gator_db::models::TaskStatus;
use gator_db::queries::tasks as db;

/// The task state machine.
///
/// Enforces the valid transition graph:
///
/// ```text
/// pending   -> assigned
/// assigned  -> running
/// running   -> checking
/// checking  -> passed
/// checking  -> failed
/// failed    -> assigned  (retry)
/// failed    -> escalated
/// escalated -> pending   (operator retry override)
/// ```
pub struct TaskStateMachine;

impl TaskStateMachine {
    /// Check whether a transition from `from` to `to` is a valid edge
    /// in the state graph.
    pub fn is_valid_transition(from: TaskStatus, to: TaskStatus) -> bool {
        matches!(
            (from, to),
            (TaskStatus::Pending, TaskStatus::Assigned)
                | (TaskStatus::Assigned, TaskStatus::Running)
                | (TaskStatus::Running, TaskStatus::Checking)
                | (TaskStatus::Checking, TaskStatus::Passed)
                | (TaskStatus::Checking, TaskStatus::Failed)
                | (TaskStatus::Failed, TaskStatus::Assigned)
                | (TaskStatus::Failed, TaskStatus::Escalated)
                | (TaskStatus::Escalated, TaskStatus::Pending)
        )
    }

    /// Execute a state transition with optimistic locking.
    ///
    /// - Validates the transition is legal.
    /// - Sets `started_at` when transitioning `assigned -> running`.
    /// - Sets `completed_at` when transitioning to `passed`, `failed`,
    ///   or `escalated`.
    /// - For `failed -> assigned` (retry), delegates to
    ///   [`Self::retry_transition`] which also increments the attempt
    ///   counter.
    ///
    /// Returns an error if:
    /// - The transition is not valid.
    /// - The current status in the database does not match `from`
    ///   (optimistic lock failure).
    /// - The task does not exist.
    pub async fn transition(
        pool: &PgPool,
        task_id: Uuid,
        from: TaskStatus,
        to: TaskStatus,
    ) -> Result<()> {
        if !Self::is_valid_transition(from, to) {
            bail!(
                "invalid state transition: {} -> {} for task {}",
                from,
                to,
                task_id
            );
        }

        // Retry is special: it increments the attempt counter.
        if from == TaskStatus::Failed && to == TaskStatus::Assigned {
            return Self::retry_transition(pool, task_id).await;
        }

        let started_at = if from == TaskStatus::Assigned && to == TaskStatus::Running {
            Some(Utc::now())
        } else {
            None
        };

        let completed_at = match to {
            TaskStatus::Passed | TaskStatus::Failed | TaskStatus::Escalated => Some(Utc::now()),
            _ => None,
        };

        let rows = db::transition_task_status(pool, task_id, from, to, started_at, completed_at)
            .await
            .with_context(|| {
                format!(
                    "failed to transition task {} from {} to {}",
                    task_id, from, to
                )
            })?;

        if rows == 0 {
            // Either the task does not exist or the status did not match.
            let task = db::get_task(pool, task_id).await?;
            match task {
                None => bail!("task {} not found", task_id),
                Some(t) => bail!(
                    "optimistic lock failed: task {} has status {}, expected {}",
                    task_id,
                    t.status,
                    from
                ),
            }
        }

        Ok(())
    }

    /// Handle the `failed -> assigned` retry transition.
    ///
    /// Fetches the task to check the attempt counter against `retry_max`,
    /// then atomically increments the attempt and resets the status.
    async fn retry_transition(pool: &PgPool, task_id: Uuid) -> Result<()> {
        let task = db::get_task(pool, task_id)
            .await?
            .with_context(|| format!("task {} not found", task_id))?;

        if task.status != TaskStatus::Failed {
            bail!(
                "cannot retry task {}: current status is {}, expected failed",
                task_id,
                task.status
            );
        }

        if task.attempt >= task.retry_max {
            bail!(
                "cannot retry task {}: attempt {} >= retry_max {}",
                task_id,
                task.attempt,
                task.retry_max
            );
        }

        let rows = db::transition_task_retry(pool, task_id, task.attempt).await?;

        if rows == 0 {
            bail!(
                "optimistic lock failed on retry for task {} (attempt {})",
                task_id,
                task.attempt
            );
        }

        Ok(())
    }

    /// Validate that all dependencies of a task are in `passed` status.
    pub async fn check_dependencies(pool: &PgPool, task_id: Uuid) -> Result<()> {
        let dep_ids = db::get_task_dependencies(pool, task_id).await?;

        for dep_id in dep_ids {
            let dep = db::get_task(pool, dep_id)
                .await?
                .with_context(|| format!("dependency task {} not found", dep_id))?;

            if dep.status != TaskStatus::Passed {
                bail!(
                    "dependency {} ({}) for task {} has status {}, expected passed",
                    dep.name,
                    dep_id,
                    task_id,
                    dep.status
                );
            }
        }

        Ok(())
    }

    /// Assign a task: validate dependencies, set harness/worktree metadata,
    /// and transition `pending -> assigned`.
    pub async fn assign_task(
        pool: &PgPool,
        task_id: Uuid,
        harness: &str,
        worktree_path: &Path,
    ) -> Result<()> {
        Self::check_dependencies(pool, task_id).await?;
        db::assign_task_metadata(
            pool,
            task_id,
            harness,
            &worktree_path.to_string_lossy(),
        )
        .await?;
        Self::transition(pool, task_id, TaskStatus::Pending, TaskStatus::Assigned).await
    }
}
