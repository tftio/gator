//! Query helpers for plan/task progress tracking.
//!
//! These re-export and wrap the lower-level DB queries from
//! [`gator_db::queries::tasks`] for use in the orchestration layer.

use anyhow::Result;
use sqlx::PgPool;
use uuid::Uuid;

use gator_db::models::Task;
pub use gator_db::queries::tasks::PlanProgress;

/// Get all tasks in a plan that are ready to be dispatched.
///
/// A task is "ready" when:
/// - Its own status is `pending`.
/// - All of its dependencies have status `passed`.
pub async fn get_ready_tasks(pool: &PgPool, plan_id: Uuid) -> Result<Vec<Task>> {
    gator_db::queries::tasks::get_ready_tasks(pool, plan_id).await
}

/// Get a progress summary (counts by status) for a plan.
pub async fn get_plan_progress(pool: &PgPool, plan_id: Uuid) -> Result<PlanProgress> {
    gator_db::queries::tasks::get_plan_progress(pool, plan_id).await
}

/// Check whether every task in a plan has status `passed`.
pub async fn is_plan_complete(pool: &PgPool, plan_id: Uuid) -> Result<bool> {
    gator_db::queries::tasks::is_plan_complete(pool, plan_id).await
}
