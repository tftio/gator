//! `gator report` command: show token usage and duration report for a plan.

use anyhow::{Context, Result};
use sqlx::PgPool;

use gator_db::queries::agent_events;
use gator_db::queries::plans as plan_db;
use gator_db::queries::tasks as task_db;

/// Run the report command.
pub async fn run_report(pool: &PgPool, plan_id_str: &str) -> Result<()> {
    let plan_id = crate::resolve::resolve_plan_id(plan_id_str)?;

    let plan = plan_db::get_plan(pool, plan_id)
        .await?
        .with_context(|| format!("plan {plan_id} not found"))?;

    // Plan summary.
    println!("Plan: {} ({})", plan.name, plan.id);
    println!("Status: {}", plan.status);

    if let (Some(approved), Some(completed)) = (plan.approved_at, plan.completed_at) {
        let duration = completed - approved;
        let secs = duration.num_seconds();
        let mins = secs / 60;
        let rem = secs % 60;
        println!("Duration: {mins}m {rem}s");
    }
    println!();

    // Total token usage.
    let (input, output) = agent_events::get_token_usage_for_plan(pool, plan_id).await?;
    let total = input + output;
    println!("Token usage:");
    println!("  Input:    {input}");
    println!("  Output:   {output}");
    println!("  Total:    {total}");
    if let Some(budget) = plan.token_budget {
        let pct = if budget > 0 {
            (total as f64 / budget as f64) * 100.0
        } else {
            0.0
        };
        println!("  Budget:   {budget} ({pct:.1}% used)");
    }
    println!();

    // Per-task breakdown.
    let tasks = task_db::list_tasks_for_plan(pool, plan_id).await?;

    let mut passed_count: usize = 0;
    let total_count = tasks.len();

    println!(
        "{:<30} {:<12} {:>8} {:>12} {:>12}",
        "TASK", "STATUS", "ATTEMPT", "TOKENS", "WALL TIME"
    );
    println!("{}", "-".repeat(76));

    for task in &tasks {
        if task.status == gator_db::models::TaskStatus::Passed {
            passed_count += 1;
        }

        let (t_input, t_output) = agent_events::get_token_usage_for_task(pool, task.id).await?;
        let t_total = t_input + t_output;
        let token_str = if t_total > 0 {
            format!("{t_total}")
        } else {
            "-".to_string()
        };

        let wall_str = match (task.started_at, task.completed_at) {
            (Some(start), Some(end)) => {
                let secs = (end - start).num_seconds();
                format!("{secs}s")
            }
            (Some(_start), None) => "running".to_string(),
            _ => "-".to_string(),
        };

        let name_display = if task.name.len() > 28 {
            format!("{}...", &task.name[..25])
        } else {
            task.name.clone()
        };

        println!(
            "{:<30} {:<12} {:>8} {:>12} {:>12}",
            name_display, task.status, task.attempt, token_str, wall_str
        );
    }

    println!();
    println!(
        "Success rate: {}/{} ({:.0}%)",
        passed_count,
        total_count,
        if total_count > 0 {
            (passed_count as f64 / total_count as f64) * 100.0
        } else {
            0.0
        }
    );

    Ok(())
}
