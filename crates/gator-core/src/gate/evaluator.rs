//! Gate verdict evaluator: translates a [`GateVerdict`] into a concrete
//! [`GateAction`] based on the task's gate policy and retry eligibility.

use anyhow::{Context, Result};
use sqlx::PgPool;
use uuid::Uuid;

use gator_db::models::GatePolicy;
use gator_db::queries::tasks as task_db;

use crate::state::dispatch;

use super::GateVerdict;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The action to take after evaluating a gate verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateAction {
    /// All invariants passed and the task has been transitioned to `passed`.
    AutoPassed,
    /// One or more invariants failed and the task has been transitioned to
    /// `failed`.
    AutoFailed {
        /// Whether the task is eligible for another retry.
        can_retry: bool,
    },
    /// The task's gate policy requires human intervention. The task remains
    /// in `checking` state.
    HumanRequired,
}

// ---------------------------------------------------------------------------
// Evaluator
// ---------------------------------------------------------------------------

/// Evaluate a gate verdict for a task and take the appropriate action.
///
/// Behavior depends on the task's `gate_policy`:
///
/// - **`auto`**: Automatically transition the task to `passed` or `failed`
///   based on the verdict. When failing, checks retry eligibility.
/// - **`human_review`** / **`human_approve`**: Leave the task in `checking`
///   state and return [`GateAction::HumanRequired`].
pub async fn evaluate_verdict(
    pool: &PgPool,
    task_id: Uuid,
    verdict: &GateVerdict,
) -> Result<GateAction> {
    let task = task_db::get_task(pool, task_id)
        .await?
        .with_context(|| format!("task {} not found", task_id))?;

    match task.gate_policy {
        GatePolicy::Auto => match verdict {
            GateVerdict::Passed => {
                dispatch::pass_task(pool, task_id).await?;
                Ok(GateAction::AutoPassed)
            }
            GateVerdict::Failed { .. } => {
                dispatch::fail_task(pool, task_id).await?;
                let can_retry = task.attempt < task.retry_max;
                Ok(GateAction::AutoFailed { can_retry })
            }
        },
        GatePolicy::HumanReview | GatePolicy::HumanApprove => {
            // Leave the task in checking state for human decision.
            Ok(GateAction::HumanRequired)
        }
    }
}
