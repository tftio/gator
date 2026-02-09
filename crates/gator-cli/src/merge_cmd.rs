//! `gator merge <plan-id>` command: merge passed task branches into the base branch.

use anyhow::{Context, Result, bail};
use sqlx::PgPool;
use uuid::Uuid;

use gator_core::worktree::{MergeResult, WorktreeManager};
use gator_db::models::{PlanStatus, TaskStatus};
use gator_db::queries::plans as plan_db;
use gator_db::queries::tasks as task_db;

/// Run the merge command.
pub async fn run_merge(pool: &PgPool, plan_id_str: &str, dry_run: bool) -> Result<()> {
    let plan_id =
        Uuid::parse_str(plan_id_str).with_context(|| format!("invalid plan ID: {plan_id_str}"))?;

    let plan = plan_db::get_plan(pool, plan_id)
        .await?
        .with_context(|| format!("plan {plan_id} not found"))?;

    if plan.status != PlanStatus::Completed {
        bail!(
            "plan {} is {} -- all tasks must pass before merging (expected completed)",
            plan_id,
            plan.status
        );
    }

    let worktree_manager =
        WorktreeManager::new(&plan.project_path, None).map_err(|e| anyhow::anyhow!("{e}"))?;

    let tasks = task_db::list_tasks_for_plan(pool, plan_id).await?;

    // Build dependency-ordered list using topological sort.
    let deps = build_dependency_map(pool, &tasks).await?;
    let ordered = topological_sort(&tasks, &deps)?;

    // Ensure we're on the base branch.
    if !dry_run {
        worktree_manager
            .checkout(&plan.base_branch)
            .map_err(|e| anyhow::anyhow!("failed to checkout {}: {e}", plan.base_branch))?;
    }

    println!(
        "Merging {} task branch(es) into {}",
        ordered.len(),
        plan.base_branch
    );

    let mut merged = 0;
    for task in &ordered {
        if task.status != TaskStatus::Passed {
            continue;
        }

        let branch = WorktreeManager::branch_name(&plan.name, &task.name);

        if dry_run {
            println!("  Would merge: {branch}");
            merged += 1;
            continue;
        }

        print!("  Merging {branch}...");
        match worktree_manager.merge_branch(&branch) {
            Ok(MergeResult::Success) => {
                println!(" ok");
                merged += 1;
            }
            Ok(MergeResult::Conflict { details }) => {
                println!(" CONFLICT");
                eprintln!("\nMerge conflict on branch {branch}:");
                eprintln!("{details}");
                eprintln!("\nStopping. Please resolve the conflict manually and re-run.");
                bail!("merge conflict on branch {branch}");
            }
            Err(e) => {
                println!(" ERROR");
                bail!("failed to merge {branch}: {e}");
            }
        }
    }

    if dry_run {
        println!("\nDry run complete: {merged} branch(es) would be merged.");
    } else {
        println!(
            "\nMerge complete: {merged} branch(es) merged into {}.",
            plan.base_branch
        );
    }

    Ok(())
}

/// Build a map of task_id -> list of dependency task_ids.
async fn build_dependency_map(
    pool: &PgPool,
    tasks: &[gator_db::models::Task],
) -> Result<std::collections::HashMap<Uuid, Vec<Uuid>>> {
    let mut deps = std::collections::HashMap::new();
    for task in tasks {
        let task_deps = task_db::get_task_dependencies(pool, task.id).await?;
        deps.insert(task.id, task_deps);
    }
    Ok(deps)
}

/// Topological sort of tasks based on dependencies.
fn topological_sort(
    tasks: &[gator_db::models::Task],
    deps: &std::collections::HashMap<Uuid, Vec<Uuid>>,
) -> Result<Vec<gator_db::models::Task>> {
    use std::collections::{HashMap, HashSet, VecDeque};

    let task_map: HashMap<Uuid, &gator_db::models::Task> =
        tasks.iter().map(|t| (t.id, t)).collect();

    // Compute in-degree (only counting edges within this task set).
    let task_ids: HashSet<Uuid> = tasks.iter().map(|t| t.id).collect();
    let mut in_degree: HashMap<Uuid, usize> = tasks.iter().map(|t| (t.id, 0)).collect();

    for task in tasks {
        if let Some(task_deps) = deps.get(&task.id) {
            for dep_id in task_deps {
                if task_ids.contains(dep_id) {
                    *in_degree.entry(task.id).or_default() += 1;
                }
            }
        }
    }

    let mut queue: VecDeque<Uuid> = in_degree
        .iter()
        .filter(|(_, deg)| **deg == 0)
        .map(|(id, _)| *id)
        .collect();

    // Stable sort: process in creation order when degrees are equal.
    let mut sorted_queue: Vec<Uuid> = queue.drain(..).collect();
    sorted_queue.sort_by_key(|id| task_map[id].created_at);
    queue.extend(sorted_queue);

    let mut result = Vec::with_capacity(tasks.len());

    // Build reverse adjacency: for each dep, which tasks depend on it.
    let mut reverse: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for task in tasks {
        if let Some(task_deps) = deps.get(&task.id) {
            for dep_id in task_deps {
                if task_ids.contains(dep_id) {
                    reverse.entry(*dep_id).or_default().push(task.id);
                }
            }
        }
    }

    while let Some(id) = queue.pop_front() {
        result.push(task_map[&id].clone());
        if let Some(dependents) = reverse.get(&id) {
            for dep in dependents {
                let deg = in_degree.get_mut(dep).unwrap();
                *deg -= 1;
                if *deg == 0 {
                    queue.push_back(*dep);
                }
            }
        }
    }

    if result.len() != tasks.len() {
        bail!("dependency cycle detected in task graph");
    }

    Ok(result)
}
