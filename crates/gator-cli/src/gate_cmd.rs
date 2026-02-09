//! `gator gate` command: view gate results for a task.

use anyhow::{Context, Result};
use sqlx::PgPool;
use uuid::Uuid;

use gator_db::queries::gate_results;
use gator_db::queries::tasks as task_db;

/// Run the gate command: show invariant results for a task's current attempt.
pub async fn run_gate(pool: &PgPool, task_id_str: &str) -> Result<()> {
    let task_id =
        Uuid::parse_str(task_id_str).with_context(|| format!("invalid task ID: {task_id_str}"))?;

    let task = task_db::get_task(pool, task_id)
        .await?
        .with_context(|| format!("task {task_id} not found"))?;

    println!("Task: {} (attempt {})", task.name, task.attempt);
    println!();

    let results = gate_results::get_latest_gate_results(pool, task_id).await?;

    if results.is_empty() {
        println!("No gate results yet.");
        return Ok(());
    }

    println!("Gate results:");
    for r in &results {
        let status = if r.passed { "PASS" } else { "FAIL" };
        let exit_str = r
            .exit_code
            .map(|c| format!("exit {c}"))
            .unwrap_or_else(|| "-".to_string());
        let duration_str = r
            .duration_ms
            .map(|ms| format!("{ms}ms"))
            .unwrap_or_else(|| "-".to_string());

        println!(
            "  [{}] {} ({}, {})",
            status, r.invariant_name, exit_str, duration_str
        );

        if !r.passed {
            if let Some(stderr) = &r.stderr {
                let snippet = stderr.trim();
                if !snippet.is_empty() {
                    let display = if snippet.len() > 200 {
                        format!("{}...", &snippet[..200])
                    } else {
                        snippet.to_string()
                    };
                    println!("    stderr: {display}");
                }
            }
        }
    }

    Ok(())
}
