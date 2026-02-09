//! Gate runner: evaluates invariants for a completed task and records the
//! verdict.
//!
//! The gate runner transitions a task into the `checking` state, executes
//! every linked invariant in the task's worktree directory, records each
//! result in the `gate_results` table, and returns a [`GateVerdict`].

pub mod evaluator;

use std::path::Path;

use anyhow::{Context, Result, bail};
use sqlx::PgPool;
use uuid::Uuid;

use gator_db::models::Invariant;
use gator_db::queries::gate_results::{self, NewGateResult};
use gator_db::queries::invariants as inv_db;
use gator_db::queries::tasks as task_db;

use crate::invariant::runner::{InvariantResult, run_invariant};
use crate::state::dispatch;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The outcome of running all gate invariants for a task.
#[derive(Debug, Clone)]
pub enum GateVerdict {
    /// Every invariant passed.
    Passed,
    /// One or more invariants failed.
    Failed {
        /// Details for each failing invariant.
        failures: Vec<GateFailure>,
    },
}

/// Information about a single invariant that failed during the gate check.
#[derive(Debug, Clone)]
pub struct GateFailure {
    /// Human-readable invariant name.
    pub invariant_name: String,
    /// The process exit code, or `None` if killed by signal.
    pub exit_code: Option<i32>,
    /// A truncated snippet of stderr output (up to 1024 bytes).
    pub stderr_snippet: String,
}

// ---------------------------------------------------------------------------
// GateRunner
// ---------------------------------------------------------------------------

/// Runs gate checks for a task by executing its linked invariants and
/// recording the results.
pub struct GateRunner<'a> {
    pool: &'a PgPool,
}

impl<'a> GateRunner<'a> {
    /// Create a new `GateRunner` backed by the given connection pool.
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    /// Run all gate checks for a task.
    ///
    /// 1. Transitions the task from `running` to `checking`.
    /// 2. Looks up all invariants linked to the task.
    /// 3. Runs each invariant in the task's worktree directory.
    /// 4. Records every result in the `gate_results` table.
    /// 5. Returns [`GateVerdict::Passed`] if all invariants passed,
    ///    or [`GateVerdict::Failed`] with details for each failure.
    pub async fn run_gate(&self, task_id: Uuid) -> Result<GateVerdict> {
        // 1. Transition to checking.
        dispatch::begin_checking(self.pool, task_id).await?;

        // 2. Look up the task to get worktree_path and attempt.
        let task = task_db::get_task(self.pool, task_id)
            .await?
            .with_context(|| format!("task {} not found", task_id))?;

        let worktree_path = task
            .worktree_path
            .as_deref()
            .with_context(|| format!("task {} has no worktree_path set", task_id))?;

        let working_dir = Path::new(worktree_path);

        // 3. Look up linked invariants.
        let invariants = inv_db::get_invariants_for_task(self.pool, task_id).await?;

        if invariants.is_empty() {
            bail!("task {} has no linked invariants; cannot run gate", task_id);
        }

        // 4. Run each invariant and collect results.
        let mut failures = Vec::new();

        for invariant in &invariants {
            let inv_result = self
                .run_and_record(task_id, task.attempt, invariant, working_dir)
                .await?;

            if !inv_result.passed {
                failures.push(GateFailure {
                    invariant_name: invariant.name.clone(),
                    exit_code: inv_result.exit_code,
                    stderr_snippet: truncate_snippet(&inv_result.stderr, 1024),
                });
            }
        }

        // 5. Return verdict.
        if failures.is_empty() {
            Ok(GateVerdict::Passed)
        } else {
            Ok(GateVerdict::Failed { failures })
        }
    }

    /// Run a single invariant and record its result in the DB.
    async fn run_and_record(
        &self,
        task_id: Uuid,
        attempt: i32,
        invariant: &Invariant,
        working_dir: &Path,
    ) -> Result<InvariantResult> {
        let result = run_invariant(invariant, working_dir).await?;

        let duration_ms = i32::try_from(result.duration_ms).unwrap_or(i32::MAX);

        let new_result = NewGateResult {
            task_id,
            invariant_id: invariant.id,
            attempt,
            passed: result.passed,
            exit_code: result.exit_code,
            stdout: Some(result.stdout.clone()),
            stderr: Some(result.stderr.clone()),
            duration_ms: Some(duration_ms),
        };

        gate_results::insert_gate_result(self.pool, &new_result)
            .await
            .with_context(|| {
                format!(
                    "failed to record gate result for invariant {:?}",
                    invariant.name
                )
            })?;

        Ok(result)
    }
}

/// Truncate a string to at most `max_bytes` bytes, appending "..." if
/// truncated.
fn truncate_snippet(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    // Find a valid UTF-8 boundary near the limit.
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = s[..end].to_owned();
    truncated.push_str("...");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_string_unchanged() {
        let s = "hello";
        assert_eq!(truncate_snippet(s, 10), "hello");
    }

    #[test]
    fn truncate_long_string_with_ellipsis() {
        let s = "abcdefghij";
        let result = truncate_snippet(s, 5);
        assert_eq!(result, "abcde...");
    }

    #[test]
    fn truncate_empty_string() {
        assert_eq!(truncate_snippet("", 10), "");
    }
}
