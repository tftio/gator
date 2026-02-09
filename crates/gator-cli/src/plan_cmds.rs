//! Operator-mode CLI handlers for `gator plan` subcommands.
//!
//! Implements:
//! - `gator plan create <file>`    -- create a plan from a TOML file
//! - `gator plan show [plan-id]`   -- show plan details or list all plans
//! - `gator plan approve <plan-id>` -- transition a plan from draft to approved

use std::collections::HashMap;

use anyhow::{Context, Result};
use sqlx::PgPool;
use uuid::Uuid;

use gator_core::plan::{
    create_plan_from_toml, get_plan_with_tasks, materialize_plan, parse_plan_toml,
};
use gator_db::queries::{invariants as inv_queries, plans as plan_queries, tasks as task_queries};

use crate::PlanCommands;

// -----------------------------------------------------------------------
// Public entry point
// -----------------------------------------------------------------------

/// Dispatch a `PlanCommands` variant to the appropriate handler.
pub async fn run_plan_command(command: PlanCommands, pool: &PgPool) -> Result<()> {
    match command {
        PlanCommands::Create { file } => cmd_create(pool, &file).await,
        PlanCommands::Show { plan_id } => match plan_id {
            Some(id) => cmd_show_one(pool, &id).await,
            None => cmd_show_all(pool).await,
        },
        PlanCommands::Approve { plan_id } => cmd_approve(pool, &plan_id).await,
        PlanCommands::Export { plan_id, output } => {
            cmd_export(pool, &plan_id, output.as_deref()).await
        }
    }
}

// -----------------------------------------------------------------------
// gator plan create <file>
// -----------------------------------------------------------------------

/// Read a plan.toml from disk, parse and validate it, insert into the DB,
/// and print a summary.
async fn cmd_create(pool: &PgPool, file_path: &str) -> Result<()> {
    // 1. Read the file.
    let content = std::fs::read_to_string(file_path)
        .with_context(|| format!("failed to read plan file: {}", file_path))?;

    // 2. Parse and validate.
    let plan_toml = parse_plan_toml(&content)
        .with_context(|| format!("failed to parse plan file: {}", file_path))?;

    // 3. Determine the project path (current working directory).
    let project_path = std::env::current_dir()
        .context("failed to get current directory")?
        .to_string_lossy()
        .to_string();

    // 4. Insert into DB.
    let (plan, warnings) = create_plan_from_toml(pool, &plan_toml, &project_path).await?;

    // 5. Count dependency edges.
    let dep_edges = task_queries::count_dependency_edges(pool, plan.id).await?;

    // 6. Print summary.
    println!("Plan created successfully.");
    println!();
    println!("  Plan ID:          {}", plan.id);
    println!("  Name:             {}", plan.name);
    println!("  Status:           {}", plan.status);
    println!("  Tasks:            {}", plan_toml.tasks.len());
    println!("  Dependency edges: {}", dep_edges);

    // 7. Print warnings (invariants not found, etc.).
    if !warnings.is_empty() {
        println!();
        println!("Warnings:");
        for w in &warnings {
            println!("  - {}", w);
        }
    }

    Ok(())
}

// -----------------------------------------------------------------------
// gator plan show (list all)
// -----------------------------------------------------------------------

/// List all plans with summary info.
async fn cmd_show_all(pool: &PgPool) -> Result<()> {
    let plans = plan_queries::list_plans(pool).await?;

    if plans.is_empty() {
        println!("No plans found. Use `gator plan create <file>` to create one.");
        return Ok(());
    }

    // Build a map of plan_id -> task count.
    let mut task_counts: HashMap<Uuid, i64> = HashMap::new();
    for plan in &plans {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM tasks WHERE plan_id = $1")
            .bind(plan.id)
            .fetch_one(pool)
            .await
            .context("failed to count tasks")?;
        task_counts.insert(plan.id, row.0);
    }

    // Compute column widths for a clean table.
    // ID is always 36 chars (UUID). Status max is 9 (completed).
    let id_w = 36;
    let name_w = plans.iter().map(|p| p.name.len()).max().unwrap_or(4).max(4);
    let status_w = 9;
    let tasks_w = 5;

    // Header
    println!(
        "{:<id_w$}  {:<name_w$}  {:<status_w$}  {:>tasks_w$}  CREATED",
        "ID", "NAME", "STATUS", "TASKS",
    );

    // Rows
    for plan in &plans {
        let count = task_counts.get(&plan.id).copied().unwrap_or(0);
        let created = plan.created_at.format("%Y-%m-%d %H:%M");
        println!(
            "{:<id_w$}  {:<name_w$}  {:<status_w$}  {:>tasks_w$}  {}",
            plan.id, plan.name, plan.status, count, created,
        );
    }

    Ok(())
}

// -----------------------------------------------------------------------
// gator plan show <plan-id>
// -----------------------------------------------------------------------

/// Show detailed info for a single plan.
async fn cmd_show_one(pool: &PgPool, plan_id_str: &str) -> Result<()> {
    let plan_id: Uuid = plan_id_str
        .parse()
        .with_context(|| format!("invalid plan ID: {:?}", plan_id_str))?;

    let (plan, tasks) = get_plan_with_tasks(pool, plan_id).await?;

    // Plan header.
    println!("Plan: {}", plan.name);
    println!("  ID:           {}", plan.id);
    println!("  Status:       {}", plan.status);
    println!("  Project:      {}", plan.project_path);
    println!("  Base branch:  {}", plan.base_branch);
    println!(
        "  Created:      {}",
        plan.created_at.format("%Y-%m-%d %H:%M:%S UTC")
    );
    if let Some(approved) = plan.approved_at {
        println!(
            "  Approved:     {}",
            approved.format("%Y-%m-%d %H:%M:%S UTC")
        );
    }
    if let Some(completed) = plan.completed_at {
        println!(
            "  Completed:    {}",
            completed.format("%Y-%m-%d %H:%M:%S UTC")
        );
    }
    println!("  Tasks:        {}", tasks.len());

    if tasks.is_empty() {
        return Ok(());
    }

    println!();
    println!("Tasks:");
    println!();

    for task in &tasks {
        // Get dependencies.
        let dep_names = task_queries::get_task_dependency_names(pool, task.id).await?;

        // Get linked invariants.
        let invariants = inv_queries::get_invariants_for_task(pool, task.id).await?;

        println!("  [{}] {}", task.status, task.name);
        println!("    ID:          {}", task.id);
        println!("    Scope:       {}", task.scope_level);
        println!("    Gate:        {}", task.gate_policy);
        println!("    Retry:       {}/{}", task.attempt, task.retry_max);

        if !dep_names.is_empty() {
            println!("    Depends on:  {}", dep_names.join(", "));
        }

        if !invariants.is_empty() {
            let inv_names: Vec<&str> = invariants.iter().map(|i| i.name.as_str()).collect();
            println!("    Invariants:  {}", inv_names.join(", "));
        }

        // Show description (indented, truncated if very long).
        let desc = task.description.trim();
        if !desc.is_empty() {
            println!("    Description:");
            for line in desc.lines().take(10) {
                println!("      {}", line);
            }
            if desc.lines().count() > 10 {
                println!("      ...(truncated)");
            }
        }

        println!();
    }

    Ok(())
}

// -----------------------------------------------------------------------
// gator plan approve <plan-id>
// -----------------------------------------------------------------------

/// Transition a plan from draft to approved.
///
/// Validates that all tasks have at least one invariant linked before
/// approving.
async fn cmd_approve(pool: &PgPool, plan_id_str: &str) -> Result<()> {
    let plan_id: Uuid = plan_id_str
        .parse()
        .with_context(|| format!("invalid plan ID: {:?}", plan_id_str))?;

    // Check that all tasks have at least one invariant.
    let tasks_without = plan_queries::count_tasks_without_invariants(pool, plan_id).await?;
    if !tasks_without.is_empty() {
        anyhow::bail!(
            "cannot approve plan: {} task(s) have no invariants linked: {}",
            tasks_without.len(),
            tasks_without.join(", "),
        );
    }

    // Perform the approval transition.
    let plan = plan_queries::approve_plan(pool, plan_id).await?;

    println!("Plan approved.");
    println!();
    println!("  Plan ID:     {}", plan.id);
    println!("  Name:        {}", plan.name);
    println!("  Status:      {}", plan.status);
    if let Some(approved) = plan.approved_at {
        println!(
            "  Approved at: {}",
            approved.format("%Y-%m-%d %H:%M:%S UTC")
        );
    }

    Ok(())
}

// -----------------------------------------------------------------------
// gator plan export <plan-id> [--output <file>]
// -----------------------------------------------------------------------

/// Materialize a plan from the database as TOML and write to a file or stdout.
async fn cmd_export(pool: &PgPool, plan_id_str: &str, output: Option<&str>) -> Result<()> {
    let plan_id: Uuid = plan_id_str
        .parse()
        .with_context(|| format!("invalid plan ID: {:?}", plan_id_str))?;

    let toml_content = materialize_plan(pool, plan_id).await?;

    match output {
        Some(path) => {
            std::fs::write(path, &toml_content)
                .with_context(|| format!("failed to write to {}", path))?;
            println!("Plan exported to {}", path);
        }
        None => {
            print!("{}", toml_content);
        }
    }

    Ok(())
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_uuid() {
        let id = "550e8400-e29b-41d4-a716-446655440000";
        let parsed: Uuid = id.parse().unwrap();
        assert_eq!(parsed.to_string(), id);
    }

    #[test]
    fn parse_invalid_uuid() {
        let id = "not-a-uuid";
        let result: Result<Uuid, _> = id.parse();
        assert!(result.is_err());
    }
}
