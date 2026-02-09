//! Agent lifecycle manager: runs a single agent task from assignment through
//! gate evaluation.
//!
//! The lifecycle function manages the full sequence: create worktree, generate
//! token, materialize task, spawn agent, collect events, run gate, evaluate
//! verdict.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use sqlx::PgPool;
use tracing;
use uuid::Uuid;

use gator_db::models::Task;
use gator_db::queries::agent_events::{self, NewAgentEvent};
use gator_db::queries::invariants as inv_db;

use crate::gate::GateRunner;
use crate::gate::evaluator::{GateAction, evaluate_verdict};
use crate::harness::Harness;
use crate::harness::types::{AgentEvent, MaterializedTask};
use crate::isolation::Isolation;
use crate::plan::materialize_task;
use crate::state::dispatch;
use crate::token::{self, TokenConfig};

/// Result of running an agent through its full lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleResult {
    /// All invariants passed.
    Passed,
    /// Invariants failed but the task is eligible for retry.
    FailedCanRetry,
    /// Invariants failed and no retries remain.
    FailedNoRetry,
    /// The task's gate policy requires human intervention.
    HumanRequired,
    /// The agent timed out.
    TimedOut,
}

/// Configuration for the agent lifecycle.
#[derive(Debug, Clone)]
pub struct LifecycleConfig {
    /// Maximum wall time for the agent to complete.
    pub timeout: Duration,
}

/// Run the full lifecycle for a single agent task.
///
/// Steps:
/// 1. Create workspace (worktree on host; container w/ copy-in for sandboxed mode)
/// 2. Generate scoped token
/// 3. Materialize task (includes retry feedback if attempt > 0)
/// 4. Build MaterializedTask with env vars
/// 5. Assign task (pending -> assigned)
/// 6. Spawn agent
/// 7. Start task (assigned -> running)
/// 8. Collect events with timeout
/// 9. Extract results from container to host worktree (no-op for worktree mode)
/// 10. Run gate on host worktree
/// 11. Evaluate verdict -> return LifecycleResult
pub async fn run_agent_lifecycle(
    pool: &PgPool,
    task: &Task,
    plan_name: &str,
    harness: &dyn Harness,
    isolation: &dyn Isolation,
    token_config: &TokenConfig,
    config: &LifecycleConfig,
) -> Result<LifecycleResult> {
    let task_id = task.id;
    let attempt = task.attempt as u32;

    tracing::info!(
        task_id = %task_id,
        task_name = %task.name,
        attempt = attempt,
        "starting agent lifecycle"
    );

    // 1. Create workspace via isolation backend.
    let workspace = isolation
        .create_workspace(plan_name, &task.name)
        .await
        .with_context(|| format!("failed to create workspace for task {}", task.name))?;

    // The path the agent sees (container: /workspace, worktree: host path).
    let agent_working_dir = workspace.path.clone();
    // The host-side path used for gate checks and commits.
    let host_worktree_path = workspace
        .host_path
        .clone()
        .unwrap_or_else(|| workspace.path.clone());

    // 2. Generate scoped token.
    let agent_token = token::generate_token(token_config, task_id, attempt);

    // 3. Materialize task description.
    let task_description = materialize_task(pool, task_id)
        .await
        .with_context(|| format!("failed to materialize task {}", task.name))?;

    // 4. Build MaterializedTask.
    let invariants = inv_db::get_invariants_for_task(pool, task_id).await?;
    let invariant_commands: Vec<String> = invariants
        .iter()
        .map(|inv| {
            if inv.args.is_empty() {
                inv.command.clone()
            } else {
                format!("{} {}", inv.command, inv.args.join(" "))
            }
        })
        .collect();

    let mut env_vars = HashMap::new();
    env_vars.insert("GATOR_AGENT_TOKEN".to_string(), agent_token);
    // Forward database URL if available.
    if let Ok(db_url) = std::env::var("GATOR_DATABASE_URL") {
        env_vars.insert("GATOR_DATABASE_URL".to_string(), db_url);
    }
    // Always forward the token secret from the resolved config.
    env_vars.insert(
        "GATOR_TOKEN_SECRET".to_string(),
        hex::encode(&token_config.secret),
    );
    // If running in a container, expose the container ID and sandbox flag.
    if let Some(ref cid) = workspace.container_id {
        env_vars.insert("GATOR_CONTAINER_ID".to_string(), cid.clone());
        env_vars.insert("GATOR_SANDBOXED".to_string(), "true".to_string());
    }

    let materialized = MaterializedTask {
        task_id,
        name: task.name.clone(),
        description: task_description,
        invariant_commands,
        working_dir: agent_working_dir,
        env_vars,
    };

    // 5. Assign task (pending -> assigned).
    // Store the host-side path so the gate runner can find the worktree.
    dispatch::assign_task(pool, task_id, harness.name(), &host_worktree_path)
        .await
        .with_context(|| format!("failed to assign task {}", task.name))?;

    // 6. Spawn agent.
    let mut handle = harness
        .spawn(&materialized)
        .await
        .with_context(|| format!("failed to spawn agent for task {}", task.name))?;

    // 6b. Write the task prompt to stdin and close it.
    //     Claude Code in `-p` mode reads the user prompt from stdin.
    if let Some(mut stdin) = handle.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let prompt = &materialized.description;
        if let Err(e) = stdin.write_all(prompt.as_bytes()).await {
            tracing::warn!(task_id = %task_id, error = %e, "failed to write prompt to agent stdin");
        }
        drop(stdin); // Close stdin so the agent starts processing.
    }

    // 7. Start task (assigned -> running).
    dispatch::start_task(pool, task_id)
        .await
        .with_context(|| format!("failed to start task {}", task.name))?;

    // 8. Collect events with timeout.
    let event_stream = harness.events(&handle);
    let collect_result = tokio::time::timeout(
        config.timeout,
        collect_events(pool, task_id, task.attempt, event_stream),
    )
    .await;

    match collect_result {
        Ok(Ok(())) => {
            tracing::info!(task_id = %task_id, "agent completed normally");
        }
        Ok(Err(e)) => {
            tracing::warn!(task_id = %task_id, error = %e, "error collecting events");
            // Continue to gate check anyway.
        }
        Err(_elapsed) => {
            tracing::warn!(task_id = %task_id, "agent timed out");
            // Kill the agent.
            if let Err(e) = harness.kill(&handle).await {
                tracing::warn!(task_id = %task_id, error = %e, "failed to kill timed-out agent");
            }
            // Transition running -> checking -> failed.
            dispatch::begin_checking(pool, task_id).await?;
            dispatch::fail_task(pool, task_id).await?;
            return Ok(LifecycleResult::TimedOut);
        }
    }

    // 9. Extract results from container (no-op for worktree isolation).
    isolation
        .extract_results(&workspace)
        .await
        .with_context(|| format!("failed to extract results for task {}", task.name))?;

    // 10. Run gate on host worktree.
    let gate_runner = GateRunner::new(pool);
    let verdict = gate_runner
        .run_gate(task_id)
        .await
        .with_context(|| format!("gate check failed for task {}", task.name))?;

    // 11. Evaluate verdict.
    let action = evaluate_verdict(pool, task_id, &verdict)
        .await
        .with_context(|| format!("failed to evaluate verdict for task {}", task.name))?;

    let result = match action {
        GateAction::AutoPassed => {
            // Commit all agent work to the worktree branch so `gator merge` can find it.
            match commit_agent_work(&host_worktree_path, &task.name, attempt) {
                Ok(true) => {
                    tracing::info!(task_id = %task_id, "committed agent work to branch");
                }
                Ok(false) => {
                    tracing::info!(task_id = %task_id, "no changes to commit");
                }
                Err(e) => {
                    tracing::warn!(task_id = %task_id, error = %e, "failed to commit agent work (non-fatal)");
                }
            }
            LifecycleResult::Passed
        }
        GateAction::AutoFailed { can_retry: true } => LifecycleResult::FailedCanRetry,
        GateAction::AutoFailed { can_retry: false } => LifecycleResult::FailedNoRetry,
        GateAction::HumanRequired => LifecycleResult::HumanRequired,
    };

    tracing::info!(
        task_id = %task_id,
        task_name = %task.name,
        result = ?result,
        "agent lifecycle completed"
    );

    Ok(result)
}

/// Commit all agent work in a worktree (git add -A + git commit).
///
/// Returns `Ok(true)` if a commit was created, `Ok(false)` if there was
/// nothing to commit, or `Err` if the git commands failed.
fn commit_agent_work(
    worktree_path: &std::path::Path,
    task_name: &str,
    attempt: u32,
) -> Result<bool> {
    use std::process::Command;

    // Configure git user for the worktree (in case it's not inherited).
    let _ = Command::new("git")
        .args(["config", "user.email", "gator@localhost"])
        .current_dir(worktree_path)
        .output();
    let _ = Command::new("git")
        .args(["config", "user.name", "gator"])
        .current_dir(worktree_path)
        .output();

    // Stage all changes.
    let output = Command::new("git")
        .args(["add", "-A"])
        .current_dir(worktree_path)
        .output()
        .with_context(|| "failed to run git add -A")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git add -A failed: {stderr}");
    }

    // Check if there is anything to commit.
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(worktree_path)
        .output()
        .with_context(|| "failed to run git status")?;

    if String::from_utf8_lossy(&status.stdout).trim().is_empty() {
        return Ok(false);
    }

    // Commit.
    let message = format!("gator: {task_name} (attempt {attempt})");
    let output = Command::new("git")
        .args(["commit", "-m", &message])
        .current_dir(worktree_path)
        .output()
        .with_context(|| "failed to run git commit")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git commit failed: {stderr}");
    }

    Ok(true)
}

/// Collect events from an agent's event stream and persist them to the DB.
///
/// Events are inserted best-effort; a failure to persist one event does not
/// stop the collection. The function returns when the stream yields
/// `AgentEvent::Completed` or the stream ends.
async fn collect_events(
    pool: &PgPool,
    task_id: Uuid,
    attempt: i32,
    mut stream: std::pin::Pin<Box<dyn futures::Stream<Item = AgentEvent> + Send>>,
) -> Result<()> {
    while let Some(event) = stream.next().await {
        let is_completed = matches!(event, AgentEvent::Completed);

        let (event_type, payload) = serialize_agent_event(&event);
        let new_event = NewAgentEvent {
            task_id,
            attempt,
            event_type,
            payload,
        };

        // Best-effort insert.
        if let Err(e) = agent_events::insert_agent_event(pool, &new_event).await {
            tracing::warn!(
                task_id = %task_id,
                error = %e,
                "failed to persist agent event (best-effort)"
            );
        }

        if is_completed {
            break;
        }
    }

    Ok(())
}

/// Serialize an AgentEvent into (event_type, payload) for DB storage.
fn serialize_agent_event(event: &AgentEvent) -> (String, serde_json::Value) {
    match event {
        AgentEvent::Message { role, content } => (
            "message".to_string(),
            serde_json::json!({"role": role, "content": content}),
        ),
        AgentEvent::ToolCall { tool, input } => (
            "tool_call".to_string(),
            serde_json::json!({"tool": tool, "input": input}),
        ),
        AgentEvent::ToolResult { tool, output } => (
            "tool_result".to_string(),
            serde_json::json!({"tool": tool, "output": output}),
        ),
        AgentEvent::TokenUsage {
            input_tokens,
            output_tokens,
        } => (
            "token_usage".to_string(),
            serde_json::json!({"input_tokens": input_tokens, "output_tokens": output_tokens}),
        ),
        AgentEvent::Error { message } => {
            ("error".to_string(), serde_json::json!({"message": message}))
        }
        AgentEvent::Completed => ("completed".to_string(), serde_json::json!({})),
    }
}
