//! Plan service layer.
//!
//! Orchestrates creating a plan from a parsed TOML definition, inserting all
//! plan data (plan row, tasks, dependencies, invariant links) within a single
//! database transaction.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use sqlx::PgPool;
use uuid::Uuid;

use gator_db::models::{Plan, Task};
use gator_db::queries::{plans as plan_queries, tasks as task_queries};

use super::toml_format::PlanToml;

/// Create a plan and all its tasks from a parsed and validated [`PlanToml`].
///
/// Inserts the plan row, all task rows, dependency edges, and invariant links
/// inside a single database transaction. If any step fails, the entire
/// operation is rolled back.
///
/// `project_path` is the filesystem path of the project this plan belongs to.
///
/// Invariant names referenced in the TOML are resolved to UUIDs by looking
/// them up in the `invariants` table. If any referenced invariant does not
/// exist, the entire operation fails and the transaction is rolled back.
pub async fn create_plan_from_toml(
    pool: &PgPool,
    plan_toml: &PlanToml,
    project_path: &str,
) -> Result<Plan> {
    let mut tx = pool.begin().await.context("failed to begin transaction")?;

    // 1. Insert the plan row.
    let plan = sqlx::query_as::<_, Plan>(
        "INSERT INTO plans (name, project_path, base_branch, token_budget, default_harness, isolation, container_image) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         RETURNING *",
    )
    .bind(&plan_toml.plan.name)
    .bind(project_path)
    .bind(&plan_toml.plan.base_branch)
    .bind(plan_toml.plan.token_budget)
    .bind(&plan_toml.plan.default_harness)
    .bind(&plan_toml.plan.isolation)
    .bind(&plan_toml.plan.container_image)
    .fetch_one(&mut *tx)
    .await
    .context("failed to insert plan")?;

    // 2. Insert all tasks and build a name -> UUID map.
    let mut task_name_to_id: HashMap<String, Uuid> = HashMap::new();

    for task_toml in &plan_toml.tasks {
        let task = sqlx::query_as::<_, Task>(
            "INSERT INTO tasks (plan_id, name, description, scope_level, gate_policy, retry_max, requested_harness) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             RETURNING *",
        )
        .bind(plan.id)
        .bind(&task_toml.name)
        .bind(&task_toml.description)
        .bind(&task_toml.scope)
        .bind(&task_toml.gate)
        .bind(task_toml.retry_max)
        .bind(&task_toml.harness)
        .fetch_one(&mut *tx)
        .await
        .with_context(|| format!("failed to insert task {:?}", task_toml.name))?;

        task_name_to_id.insert(task_toml.name.clone(), task.id);
    }

    // 3. Insert dependency edges.
    for task_toml in &plan_toml.tasks {
        let task_id = task_name_to_id[&task_toml.name];
        for dep_name in &task_toml.depends_on {
            let dep_id = task_name_to_id[dep_name];
            sqlx::query(
                "INSERT INTO task_dependencies (task_id, depends_on) VALUES ($1, $2) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(task_id)
            .bind(dep_id)
            .execute(&mut *tx)
            .await
            .with_context(|| {
                format!(
                    "failed to insert dependency: {:?} -> {:?}",
                    task_toml.name, dep_name
                )
            })?;
        }
    }

    // 4. Link invariants by name (look up each name in the invariants table).
    let mut missing: Vec<String> = Vec::new();

    for task_toml in &plan_toml.tasks {
        let task_id = task_name_to_id[&task_toml.name];
        for inv_name in &task_toml.invariants {
            let inv_row: Option<(Uuid,)> =
                sqlx::query_as("SELECT id FROM invariants WHERE name = $1")
                    .bind(inv_name)
                    .fetch_optional(&mut *tx)
                    .await
                    .with_context(|| format!("failed to look up invariant {:?}", inv_name))?;

            match inv_row {
                Some((inv_id,)) => {
                    sqlx::query(
                        "INSERT INTO task_invariants (task_id, invariant_id) VALUES ($1, $2) \
                         ON CONFLICT DO NOTHING",
                    )
                    .bind(task_id)
                    .bind(inv_id)
                    .execute(&mut *tx)
                    .await
                    .with_context(|| {
                        format!(
                            "failed to link task {:?} to invariant {:?}",
                            task_toml.name, inv_name
                        )
                    })?;
                }
                None => {
                    missing.push(format!(
                        "invariant {:?} referenced by task {:?} does not exist in the database",
                        inv_name, task_toml.name
                    ));
                }
            }
        }
    }

    if !missing.is_empty() {
        // Transaction rolls back on drop (no commit).
        bail!(
            "plan references unknown invariants:\n  {}",
            missing.join("\n  ")
        );
    }

    tx.commit().await.context("failed to commit transaction")?;

    Ok(plan)
}

/// Fetch a plan and all its tasks.
pub async fn get_plan_with_tasks(pool: &PgPool, plan_id: Uuid) -> Result<(Plan, Vec<Task>)> {
    let plan = plan_queries::get_plan(pool, plan_id)
        .await?
        .with_context(|| format!("plan {plan_id} not found"))?;

    let tasks = task_queries::list_tasks_for_plan(pool, plan_id).await?;

    Ok((plan, tasks))
}
