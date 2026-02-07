//! Database query functions for the `gate_results` table.

use anyhow::{Context, Result};
use sqlx::PgPool;
use uuid::Uuid;

use crate::models::GateResult;

/// Parameters for inserting a new gate result row.
#[derive(Debug, Clone)]
pub struct NewGateResult {
    pub task_id: Uuid,
    pub invariant_id: Uuid,
    pub attempt: i32,
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub duration_ms: Option<i32>,
}

/// Insert a new gate result row. Returns the inserted row with
/// server-generated defaults (id, checked_at).
pub async fn insert_gate_result(pool: &PgPool, new: &NewGateResult) -> Result<GateResult> {
    let result = sqlx::query_as::<_, GateResult>(
        "INSERT INTO gate_results \
         (task_id, invariant_id, attempt, passed, exit_code, stdout, stderr, duration_ms) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         RETURNING *",
    )
    .bind(new.task_id)
    .bind(new.invariant_id)
    .bind(new.attempt)
    .bind(new.passed)
    .bind(new.exit_code)
    .bind(&new.stdout)
    .bind(&new.stderr)
    .bind(new.duration_ms)
    .fetch_one(pool)
    .await
    .with_context(|| {
        format!(
            "failed to insert gate result for task {} invariant {} attempt {}",
            new.task_id, new.invariant_id, new.attempt
        )
    })?;

    Ok(result)
}

/// Get all gate results for a given task and attempt, ordered by
/// invariant name (via checked_at as a proxy for insertion order).
pub async fn get_gate_results(
    pool: &PgPool,
    task_id: Uuid,
    attempt: i32,
) -> Result<Vec<GateResult>> {
    let results = sqlx::query_as::<_, GateResult>(
        "SELECT * FROM gate_results \
         WHERE task_id = $1 AND attempt = $2 \
         ORDER BY checked_at ASC",
    )
    .bind(task_id)
    .bind(attempt)
    .fetch_all(pool)
    .await
    .with_context(|| {
        format!(
            "failed to get gate results for task {} attempt {}",
            task_id, attempt
        )
    })?;

    Ok(results)
}
