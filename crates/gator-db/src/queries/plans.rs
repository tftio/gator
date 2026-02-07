//! Database query functions for the `plans` table.

use anyhow::{Context, Result};
use sqlx::PgPool;
use uuid::Uuid;

use crate::models::{Plan, PlanStatus};

/// Insert a new plan row. Returns the inserted plan with server-generated
/// defaults (id, created_at, status).
pub async fn insert_plan(
    pool: &PgPool,
    name: &str,
    project_path: &str,
    base_branch: &str,
) -> Result<Plan> {
    let plan = sqlx::query_as::<_, Plan>(
        "INSERT INTO plans (name, project_path, base_branch) \
         VALUES ($1, $2, $3) \
         RETURNING *",
    )
    .bind(name)
    .bind(project_path)
    .bind(base_branch)
    .fetch_one(pool)
    .await
    .context("failed to insert plan")?;

    Ok(plan)
}

/// Fetch a plan by its ID.
pub async fn get_plan(pool: &PgPool, id: Uuid) -> Result<Option<Plan>> {
    let plan = sqlx::query_as::<_, Plan>("SELECT * FROM plans WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .context("failed to fetch plan")?;

    Ok(plan)
}

/// List all plans, ordered by creation time (newest first).
pub async fn list_plans(pool: &PgPool) -> Result<Vec<Plan>> {
    let plans = sqlx::query_as::<_, Plan>("SELECT * FROM plans ORDER BY created_at DESC")
        .fetch_all(pool)
        .await
        .context("failed to list plans")?;

    Ok(plans)
}

/// Update the status of a plan.
pub async fn update_plan_status(pool: &PgPool, id: Uuid, status: PlanStatus) -> Result<()> {
    let result = sqlx::query("UPDATE plans SET status = $1 WHERE id = $2")
        .bind(status)
        .bind(id)
        .execute(pool)
        .await
        .context("failed to update plan status")?;

    if result.rows_affected() == 0 {
        anyhow::bail!("plan {id} not found");
    }

    Ok(())
}

/// Transition a plan from `draft` to `approved`, setting `approved_at` to now.
///
/// Returns the updated plan. Fails if the plan is not found or is not in
/// `draft` status.
pub async fn approve_plan(pool: &PgPool, id: Uuid) -> Result<Plan> {
    let plan = sqlx::query_as::<_, Plan>(
        "UPDATE plans \
         SET status = 'approved', approved_at = now() \
         WHERE id = $1 AND status = 'draft' \
         RETURNING *",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("failed to approve plan")?;

    match plan {
        Some(p) => Ok(p),
        None => {
            // Distinguish between "not found" and "wrong status".
            let existing = get_plan(pool, id).await?;
            match existing {
                None => anyhow::bail!("plan {id} not found"),
                Some(p) => anyhow::bail!(
                    "plan {id} cannot be approved: current status is {:?} (must be draft)",
                    p.status.to_string()
                ),
            }
        }
    }
}

/// Count tasks in a plan that have zero linked invariants.
pub async fn count_tasks_without_invariants(pool: &PgPool, plan_id: Uuid) -> Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT t.name FROM tasks t \
         WHERE t.plan_id = $1 \
           AND NOT EXISTS ( \
               SELECT 1 FROM task_invariants ti WHERE ti.task_id = t.id \
           ) \
         ORDER BY t.name",
    )
    .bind(plan_id)
    .fetch_all(pool)
    .await
    .context("failed to count tasks without invariants")?;

    Ok(rows.into_iter().map(|(name,)| name).collect())
}
