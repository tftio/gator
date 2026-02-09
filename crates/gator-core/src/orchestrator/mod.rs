//! DAG-aware orchestrator: runs a plan to completion by spawning agents in
//! topological order, enforcing concurrency limits, and handling retries.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use sqlx::PgPool;
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use gator_db::models::{PlanStatus, TaskStatus};
use gator_db::queries::agent_events;
use gator_db::queries::plans as plan_db;
use gator_db::queries::tasks as task_db;

use crate::harness::HarnessRegistry;
use crate::isolation::Isolation;
use crate::lifecycle::{run_agent_lifecycle, LifecycleConfig, LifecycleResult};
use crate::state::dispatch;
use crate::token::TokenConfig;

/// Configuration for the orchestrator.
#[derive(Debug, Clone)]
pub struct OrchestratorConfig {
    /// Maximum number of concurrent agents.
    pub max_agents: usize,
    /// Wall time limit per task.
    pub task_timeout: Duration,
}

/// Result of running the orchestrator to completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrchestratorResult {
    /// All tasks passed successfully.
    Completed,
    /// One or more tasks failed (exhausted retries or escalated).
    Failed { failed_tasks: Vec<String> },
    /// One or more tasks require human review.
    HumanRequired { tasks_awaiting_review: Vec<String> },
    /// Token budget exceeded.
    BudgetExceeded { used: i64, budget: i64 },
    /// Orchestrator was interrupted by a cancellation signal.
    Interrupted,
}

/// Message sent from spawned lifecycle tasks back to the orchestrator loop.
struct LifecycleDone {
    task_id: Uuid,
    task_name: String,
    result: Result<LifecycleResult>,
}

/// Retry a failed task back to pending so the DAG scheduler picks it up.
///
/// Checks retry eligibility and uses `retry_task_to_pending` which resets
/// the task to `pending` (unlike `dispatch::retry_task` which sets `assigned`).
async fn orchestrator_retry(pool: &PgPool, task_id: Uuid) -> Result<()> {
    let task = task_db::get_task(pool, task_id)
        .await?
        .with_context(|| format!("task {} not found", task_id))?;

    if task.status != TaskStatus::Failed {
        bail!(
            "cannot retry task {}: status is {}, expected failed",
            task_id,
            task.status
        );
    }

    if task.attempt >= task.retry_max {
        bail!(
            "cannot retry task {}: attempt {} >= retry_max {}",
            task_id,
            task.attempt,
            task.retry_max
        );
    }

    let rows = task_db::retry_task_to_pending(pool, task_id, task.attempt).await?;
    if rows == 0 {
        bail!(
            "optimistic lock failed on retry-to-pending for task {}",
            task_id
        );
    }

    Ok(())
}

/// Run the orchestrator for a plan.
///
/// Spawns agents in DAG order (tasks whose dependencies are all passed),
/// enforces a concurrency limit via a semaphore, retries failures when
/// eligible, and escalates when retries are exhausted.
pub async fn run_orchestrator(
    pool: &PgPool,
    plan_id: Uuid,
    registry: &Arc<HarnessRegistry>,
    isolation: &Arc<dyn Isolation>,
    token_config: &TokenConfig,
    config: &OrchestratorConfig,
    cancel: CancellationToken,
) -> Result<OrchestratorResult> {
    // Look up the plan.
    let plan = plan_db::get_plan(pool, plan_id)
        .await?
        .with_context(|| format!("plan {} not found", plan_id))?;

    let plan_name = plan.name.clone();
    let default_harness = plan.default_harness.clone();

    // 1. Restart recovery: reset orphaned tasks.
    let orphaned = task_db::reset_orphaned_tasks(pool, plan_id).await?;
    for orphan in &orphaned {
        tracing::warn!(
            task_id = %orphan.id,
            task_name = %orphan.name,
            "reset orphaned task to failed"
        );
    }

    // Handle orphaned tasks: retry if eligible, escalate otherwise.
    for orphan in &orphaned {
        if orphan.attempt < orphan.retry_max {
            orchestrator_retry(pool, orphan.id).await?;
            tracing::info!(
                task_id = %orphan.id,
                task_name = %orphan.name,
                "retrying orphaned task"
            );
        } else {
            dispatch::escalate_task(pool, orphan.id).await?;
            tracing::warn!(
                task_id = %orphan.id,
                task_name = %orphan.name,
                "escalating orphaned task (no retries left)"
            );
        }
    }

    // 2. Plan status: approved -> running (skip if already running).
    if plan.status == PlanStatus::Approved {
        plan_db::update_plan_status(pool, plan_id, PlanStatus::Running).await?;
    } else if plan.status != PlanStatus::Running {
        bail!(
            "plan {} has status {}, expected approved or running",
            plan_id,
            plan.status
        );
    }

    // 3. Main orchestration loop.
    let semaphore = Arc::new(Semaphore::new(config.max_agents));
    let (tx, mut rx) = mpsc::channel::<LifecycleDone>(config.max_agents * 2);
    let mut in_flight: usize = 0;

    loop {
        // 3-pre. Check cancellation.
        if cancel.is_cancelled() {
            tracing::info!(plan_id = %plan_id, "orchestrator cancelled, draining in-flight tasks");
            let drain_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while in_flight > 0 {
                match tokio::time::timeout_at(drain_deadline, rx.recv()).await {
                    Ok(Some(done)) => {
                        in_flight -= 1;
                        let _ = handle_lifecycle_result(pool, &done).await;
                    }
                    _ => break,
                }
            }
            if in_flight > 0 {
                tracing::warn!(
                    plan_id = %plan_id,
                    remaining = in_flight,
                    "drain timeout expired, {} tasks still in flight",
                    in_flight
                );
            }
            plan_db::update_plan_status(pool, plan_id, PlanStatus::Failed).await?;
            return Ok(OrchestratorResult::Interrupted);
        }

        // 3a. Drain completed results (non-blocking).
        while let Ok(done) = rx.try_recv() {
            in_flight -= 1;
            handle_lifecycle_result(pool, &done).await?;
        }

        // 3a-bis. Budget check.
        if let Some(budget) = plan.token_budget {
            let (input, output) = agent_events::get_token_usage_for_plan(pool, plan_id).await?;
            let total = input + output;
            if total >= budget {
                tracing::warn!(
                    plan_id = %plan_id,
                    used = total,
                    budget = budget,
                    "token budget exceeded, stopping plan"
                );
                plan_db::update_plan_status(pool, plan_id, PlanStatus::Failed).await?;
                return Ok(OrchestratorResult::BudgetExceeded {
                    used: total,
                    budget,
                });
            }
        }

        // 3b. Check termination conditions.
        let is_complete = task_db::is_plan_complete(pool, plan_id).await?;
        if is_complete {
            plan_db::update_plan_status(pool, plan_id, PlanStatus::Completed).await?;
            return Ok(OrchestratorResult::Completed);
        }

        let progress = task_db::get_plan_progress(pool, plan_id).await?;

        // All non-passed tasks are either escalated or checking (human review).
        if progress.pending == 0
            && progress.assigned == 0
            && progress.running == 0
            && progress.failed == 0
            && in_flight == 0
        {
            let tasks = task_db::list_tasks_for_plan(pool, plan_id).await?;
            let escalated: Vec<String> = tasks
                .iter()
                .filter(|t| t.status == TaskStatus::Escalated)
                .map(|t| t.name.clone())
                .collect();
            let checking: Vec<String> = tasks
                .iter()
                .filter(|t| t.status == TaskStatus::Checking)
                .map(|t| t.name.clone())
                .collect();

            if !checking.is_empty() {
                // Leave plan as Running so the operator can approve/reject
                // tasks and re-run `gator dispatch` to resume.
                return Ok(OrchestratorResult::HumanRequired {
                    tasks_awaiting_review: checking,
                });
            }
            if !escalated.is_empty() {
                plan_db::update_plan_status(pool, plan_id, PlanStatus::Failed).await?;
                return Ok(OrchestratorResult::Failed {
                    failed_tasks: escalated,
                });
            }
        }

        // 3c. Handle any failed tasks (retry or escalate).
        if progress.failed > 0 && in_flight == 0 {
            let tasks = task_db::list_tasks_for_plan(pool, plan_id).await?;
            for task in &tasks {
                if task.status == TaskStatus::Failed {
                    if task.attempt < task.retry_max {
                        orchestrator_retry(pool, task.id).await?;
                    } else {
                        dispatch::escalate_task(pool, task.id).await?;
                    }
                }
            }
            // Continue to spawn ready tasks in the next iteration.
            continue;
        }

        // 3d. Spawn ready tasks.
        let ready = task_db::get_ready_tasks(pool, plan_id).await?;
        let spawned_any = !ready.is_empty();

        for task in ready {
            // Acquire semaphore permit.
            let permit = semaphore.clone().acquire_owned().await?;

            let pool_clone = pool.clone();
            let plan_name_clone = plan_name.clone();
            let registry_clone = Arc::clone(registry);
            let isolation_clone = Arc::clone(isolation);
            let token_cfg = token_config.clone();
            let lifecycle_config = LifecycleConfig {
                timeout: config.task_timeout,
            };
            let tx_clone = tx.clone();
            let task_name = task.name.clone();
            let task_id = task.id;

            // Choose harness: per-task > plan default > first registered.
            let preferred = task.requested_harness.clone()
                .unwrap_or_else(|| default_harness.clone());

            let harness_name = if registry_clone.get(&preferred).is_some() {
                preferred
            } else if let Some(first) = registry_clone.list().first() {
                tracing::warn!(
                    task_name = %task.name,
                    preferred = %preferred,
                    fallback = %first,
                    "preferred harness not found, falling back to first registered"
                );
                first.to_string()
            } else {
                tracing::error!(
                    task_name = %task.name,
                    "no harnesses registered, skipping task"
                );
                continue;
            };

            in_flight += 1;

            tokio::spawn(async move {
                let Some(harness) = registry_clone.get(&harness_name) else {
                    tracing::error!(
                        task_id = %task_id,
                        harness = %harness_name,
                        "harness disappeared from registry after validation"
                    );
                    drop(permit);
                    let _ = tx_clone
                        .send(LifecycleDone {
                            task_id,
                            task_name,
                            result: Err(anyhow::anyhow!(
                                "harness '{}' not found in registry",
                                harness_name
                            )),
                        })
                        .await;
                    return;
                };

                let result = run_agent_lifecycle(
                    &pool_clone,
                    &task,
                    &plan_name_clone,
                    harness,
                    isolation_clone.as_ref(),
                    &token_cfg,
                    &lifecycle_config,
                )
                .await;

                // Release semaphore permit.
                drop(permit);

                // Send result back.
                let _ = tx_clone
                    .send(LifecycleDone {
                        task_id,
                        task_name,
                        result,
                    })
                    .await;
            });
        }

        // 3e. If tasks are in flight but nothing is ready, wait for a result
        // or cancellation.
        if in_flight > 0 {
            tokio::select! {
                done = rx.recv() => {
                    if let Some(done) = done {
                        in_flight -= 1;
                        handle_lifecycle_result(pool, &done).await?;
                    }
                }
                _ = cancel.cancelled() => {
                    // Will be handled at top of next loop iteration.
                    continue;
                }
            }
        } else if !spawned_any {
            // Nothing in flight, nothing spawned this iteration.
            // Brief sleep to avoid busy-loop before re-checking.
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                _ = cancel.cancelled() => {
                    continue;
                }
            }
        }
    }
}

/// Handle the result of a completed lifecycle.
async fn handle_lifecycle_result(pool: &PgPool, done: &LifecycleDone) -> Result<()> {
    match &done.result {
        Ok(LifecycleResult::Passed) => {
            tracing::info!(
                task_id = %done.task_id,
                task_name = %done.task_name,
                "task passed"
            );
        }
        Ok(LifecycleResult::FailedCanRetry) => {
            tracing::info!(
                task_id = %done.task_id,
                task_name = %done.task_name,
                "task failed, will retry on next loop iteration"
            );
            // Task is in `failed` state. The main loop will handle retry via
            // orchestrator_retry (which resets to pending for DAG scheduling).
        }
        Ok(LifecycleResult::FailedNoRetry) => {
            tracing::warn!(
                task_id = %done.task_id,
                task_name = %done.task_name,
                "task failed, no retries left, escalating"
            );
            dispatch::escalate_task(pool, done.task_id).await?;
        }
        Ok(LifecycleResult::TimedOut) => {
            tracing::warn!(
                task_id = %done.task_id,
                task_name = %done.task_name,
                "task timed out"
            );
            // Task is already in `failed` state from lifecycle timeout handler.
            // The main loop will handle retry or escalation.
        }
        Ok(LifecycleResult::HumanRequired) => {
            tracing::info!(
                task_id = %done.task_id,
                task_name = %done.task_name,
                "task requires human review"
            );
            // Task stays in checking state.
        }
        Err(e) => {
            tracing::error!(
                task_id = %done.task_id,
                task_name = %done.task_name,
                error = %e,
                "lifecycle failed with error"
            );
            // Try to transition the task to failed for cleanup.
            let task = task_db::get_task(pool, done.task_id).await?;
            if let Some(task) = task {
                match task.status {
                    TaskStatus::Running => {
                        let _ = dispatch::begin_checking(pool, done.task_id).await;
                        let _ = dispatch::fail_task(pool, done.task_id).await;
                    }
                    TaskStatus::Checking => {
                        let _ = dispatch::fail_task(pool, done.task_id).await;
                    }
                    TaskStatus::Assigned => {
                        // Error during spawn (before start_task). Force to
                        // running then through checking -> failed so the
                        // state machine stays consistent.
                        let _ = dispatch::start_task(pool, done.task_id).await;
                        let _ = dispatch::begin_checking(pool, done.task_id).await;
                        let _ = dispatch::fail_task(pool, done.task_id).await;
                    }
                    TaskStatus::Pending => {
                        // Error before assign_task even ran (e.g. worktree
                        // creation). Force through the full state chain so
                        // retry/escalation can proceed.
                        let _ = dispatch::assign_task(
                            pool,
                            done.task_id,
                            "error-recovery",
                            std::path::Path::new("/dev/null"),
                        )
                        .await;
                        let _ = dispatch::start_task(pool, done.task_id).await;
                        let _ = dispatch::begin_checking(pool, done.task_id).await;
                        let _ = dispatch::fail_task(pool, done.task_id).await;
                    }
                    _ => {}
                }
                // The main loop will handle retry/escalation for failed tasks.
            }
        }
    }

    Ok(())
}
