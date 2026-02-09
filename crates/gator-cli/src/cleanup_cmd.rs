//! `gator cleanup <plan-id>` command: remove worktrees for completed tasks.

use anyhow::{Context, Result};
use sqlx::PgPool;
use uuid::Uuid;

use gator_core::worktree::WorktreeManager;
use gator_db::models::TaskStatus;
use gator_db::queries::plans as plan_db;
use gator_db::queries::tasks as task_db;

/// Run the cleanup command.
pub async fn run_cleanup(pool: &PgPool, plan_id_str: &str, all: bool) -> Result<()> {
    let plan_id =
        Uuid::parse_str(plan_id_str).with_context(|| format!("invalid plan ID: {plan_id_str}"))?;

    let plan = plan_db::get_plan(pool, plan_id)
        .await?
        .with_context(|| format!("plan {plan_id} not found"))?;

    let worktree_manager =
        WorktreeManager::new(&plan.project_path, None).map_err(|e| anyhow::anyhow!("{e}"))?;

    let tasks = task_db::list_tasks_for_plan(pool, plan_id).await?;

    let mut removed = 0;
    let mut skipped = 0;

    for task in &tasks {
        let dominated_by_policy = if all {
            true
        } else {
            task.status == TaskStatus::Passed
        };

        if !dominated_by_policy {
            skipped += 1;
            continue;
        }

        if let Some(ref wt_path) = task.worktree_path {
            let path = std::path::Path::new(wt_path);
            match worktree_manager.remove_worktree(path) {
                Ok(()) => {
                    println!("  Removed: {} ({})", task.name, wt_path);
                    removed += 1;
                }
                Err(e) => {
                    eprintln!(
                        "  Warning: failed to remove worktree for {}: {e}",
                        task.name
                    );
                }
            }
        }
    }

    // Prune any stale worktree references.
    let _ = worktree_manager.cleanup_stale();

    println!("\nCleanup complete: {removed} worktree(s) removed, {skipped} skipped.");

    Ok(())
}
