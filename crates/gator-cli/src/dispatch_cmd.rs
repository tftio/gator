//! `gator dispatch` command: run a plan to completion using the orchestrator.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::PgPool;
use uuid::Uuid;

use gator_core::harness::{ClaudeCodeAdapter, HarnessRegistry};
use gator_core::orchestrator::{run_orchestrator, OrchestratorConfig, OrchestratorResult};
use gator_core::token::TokenConfig;
use gator_core::worktree::WorktreeManager;
use gator_db::queries::plans as plan_db;

/// Run the dispatch command.
pub async fn run_dispatch(
    pool: &PgPool,
    plan_id_str: &str,
    max_agents: usize,
    timeout_secs: u64,
    token_config: &TokenConfig,
) -> Result<()> {
    // Parse plan ID.
    let plan_id = Uuid::parse_str(plan_id_str)
        .with_context(|| format!("invalid plan ID: {plan_id_str}"))?;

    // Load plan to get project_path.
    let plan = plan_db::get_plan(pool, plan_id)
        .await?
        .with_context(|| format!("plan {plan_id} not found"))?;

    println!("Dispatching plan: {} ({})", plan.name, plan.id);
    println!("  Max agents: {max_agents}");
    println!("  Task timeout: {timeout_secs}s");

    // Set up harness registry.
    let mut registry = HarnessRegistry::new();
    registry.register(ClaudeCodeAdapter::new());
    let registry = Arc::new(registry);

    // Set up worktree manager.
    let worktree_manager = WorktreeManager::new(&plan.project_path, None)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Build config.
    let config = OrchestratorConfig {
        max_agents,
        task_timeout: Duration::from_secs(timeout_secs),
    };

    // Run orchestrator.
    let result = run_orchestrator(
        pool,
        plan_id,
        &registry,
        &worktree_manager,
        token_config,
        &config,
    )
    .await?;

    // Print result.
    match result {
        OrchestratorResult::Completed => {
            println!("\nPlan completed successfully! All tasks passed.");
        }
        OrchestratorResult::Failed { failed_tasks } => {
            println!("\nPlan failed. Escalated tasks:");
            for task in &failed_tasks {
                println!("  - {task}");
            }
            std::process::exit(1);
        }
        OrchestratorResult::HumanRequired {
            tasks_awaiting_review,
        } => {
            println!("\nPlan requires human review. Tasks awaiting review:");
            for task in &tasks_awaiting_review {
                println!("  - {task}");
            }
            std::process::exit(2);
        }
    }

    Ok(())
}
