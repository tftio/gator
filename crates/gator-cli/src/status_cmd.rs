//! `gator status` command: show plan progress and per-task status.

use anyhow::{Context, Result};
use sqlx::PgPool;
use uuid::Uuid;

use gator_db::queries::plans as plan_db;
use gator_db::queries::tasks as task_db;

/// Run the status command.
pub async fn run_status(pool: &PgPool, plan_id_str: &str) -> Result<()> {
    let plan_id = Uuid::parse_str(plan_id_str)
        .with_context(|| format!("invalid plan ID: {plan_id_str}"))?;

    let plan = plan_db::get_plan(pool, plan_id)
        .await?
        .with_context(|| format!("plan {plan_id} not found"))?;

    println!("Plan: {} ({})", plan.name, plan.id);
    println!("Status: {}", plan.status);
    if let Some(approved_at) = plan.approved_at {
        println!("Approved: {}", approved_at.format("%Y-%m-%d %H:%M:%S UTC"));
    }
    if let Some(completed_at) = plan.completed_at {
        println!("Completed: {}", completed_at.format("%Y-%m-%d %H:%M:%S UTC"));
    }
    println!();

    // Progress summary.
    let progress = task_db::get_plan_progress(pool, plan_id).await?;
    println!("Progress: {}/{} passed", progress.passed, progress.total);
    println!(
        "  pending={} assigned={} running={} checking={} passed={} failed={} escalated={}",
        progress.pending,
        progress.assigned,
        progress.running,
        progress.checking,
        progress.passed,
        progress.failed,
        progress.escalated,
    );
    println!();

    // Per-task listing.
    let tasks = task_db::list_tasks_for_plan(pool, plan_id).await?;
    println!("Tasks:");
    for task in &tasks {
        let status_icon = match task.status.to_string().as_str() {
            "pending" => ".",
            "assigned" => ">",
            "running" => "*",
            "checking" => "?",
            "passed" => "+",
            "failed" => "!",
            "escalated" => "X",
            _ => " ",
        };
        println!(
            "  [{}] {} (attempt {}, {})",
            status_icon, task.name, task.attempt, task.status
        );
    }

    Ok(())
}
