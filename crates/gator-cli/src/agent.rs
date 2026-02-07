//! Agent-mode CLI command implementations.
//!
//! When `GATOR_AGENT_TOKEN` is set in the environment, the CLI restricts
//! its command surface to the four agent-mode commands: `task`, `check`,
//! `progress`, and `done`. This module contains the dispatch logic and
//! each command's implementation.
//!
//! All commands validate the scoped token before doing any work. The token
//! encodes a (task_id, attempt) pair that scopes the agent to exactly one
//! task.

use std::process::Stdio;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use gator_core::token::guard::{self, GuardError};
use gator_core::token::{TokenClaims, TokenConfig};
use gator_db::models::Invariant;
use sqlx::PgPool;

use crate::Commands;

// -----------------------------------------------------------------------
// Public entry point
// -----------------------------------------------------------------------

/// Run the CLI in agent mode.
///
/// This is called when `GATOR_AGENT_TOKEN` is detected in the environment.
/// It validates the token, restricts the command surface, and dispatches to
/// the appropriate agent-mode handler.
///
/// # Errors
///
/// Returns an error (and the process should exit non-zero) when:
/// - The token is missing or invalid
/// - An operator-only command was attempted
/// - A DB query fails
/// - An invariant check fails (for `gator check`)
pub async fn run_agent_mode(
    command: Commands,
    pool: Option<&PgPool>,
) -> Result<()> {
    // Validate the token first.
    let token_config = TokenConfig::from_env()
        .context("GATOR_TOKEN_SECRET must be set for agent mode")?;
    let claims = guard::require_agent_mode(&token_config)
        .map_err(|e| match e {
            GuardError::InvalidToken(inner) => {
                anyhow::anyhow!("invalid agent token: {inner}")
            }
            other => anyhow::anyhow!("{other}"),
        })?;

    match command {
        Commands::Task => cmd_task(&claims, pool).await,
        Commands::Check => cmd_check(&claims, pool).await,
        Commands::Progress { message } => cmd_progress(&claims, pool, &message).await,
        Commands::Done => cmd_done(&claims, pool).await,
        // Any operator command is blocked in agent mode.
        _ => {
            bail!("Error: this command is not available in agent mode");
        }
    }
}

// -----------------------------------------------------------------------
// gator task
// -----------------------------------------------------------------------

/// `gator task` -- read the assigned task description.
///
/// Validates the token, looks up the task in the DB, and prints a clean
/// markdown description that the LLM agent can consume.
async fn cmd_task(claims: &TokenClaims, pool: Option<&PgPool>) -> Result<()> {
    let pool = require_db(pool)?;

    // Look up the task by ID.
    let task: gator_db::models::Task = sqlx::query_as(
        "SELECT id, plan_id, name, description, scope_level, gate_policy, \
         retry_max, status, assigned_harness, worktree_path, attempt, \
         created_at, started_at, completed_at \
         FROM tasks WHERE id = $1",
    )
    .bind(claims.task_id)
    .fetch_optional(pool)
    .await
    .context("failed to query task")?
    .with_context(|| format!("task {} not found", claims.task_id))?;

    // Look up linked invariants for this task.
    let invariants: Vec<Invariant> = sqlx::query_as(
        "SELECT i.id, i.name, i.description, i.kind, i.command, i.args, \
         i.expected_exit_code, i.threshold, i.scope, i.created_at \
         FROM invariants i \
         INNER JOIN task_invariants ti ON ti.invariant_id = i.id \
         WHERE ti.task_id = $1 \
         ORDER BY i.name",
    )
    .bind(claims.task_id)
    .fetch_all(pool)
    .await
    .context("failed to query invariants for task")?;

    // Print as clean markdown.
    println!("# Task: {}", task.name);
    println!();
    println!("{}", task.description);
    println!();
    println!("## Details");
    println!();
    println!("- **Scope**: {}", task.scope_level);
    println!("- **Gate policy**: {}", task.gate_policy);
    println!("- **Attempt**: {}/{}", claims.attempt, task.retry_max);
    println!("- **Status**: {}", task.status);
    println!();

    if !invariants.is_empty() {
        println!("## Invariant commands");
        println!();
        println!("Run `gator check` to execute all invariants, or run individually:");
        println!();
        for inv in &invariants {
            let args_str = if inv.args.is_empty() {
                String::new()
            } else {
                format!(" {}", inv.args.join(" "))
            };
            println!("- **{}**: `{}{}`", inv.name, inv.command, args_str);
        }
        println!();
    } else {
        println!("_No invariants linked to this task._");
        println!();
    }

    Ok(())
}

// -----------------------------------------------------------------------
// gator check
// -----------------------------------------------------------------------

/// Result of running a single invariant check.
struct InvariantCheckResult {
    name: String,
    passed: bool,
    exit_code: i32,
    stdout_snippet: String,
    stderr_snippet: String,
    duration_ms: u64,
}

/// `gator check` -- run all invariants linked to this task.
///
/// Looks up linked invariants, executes each in the current working
/// directory, prints results, and exits 0 only if ALL pass.
async fn cmd_check(claims: &TokenClaims, pool: Option<&PgPool>) -> Result<()> {
    let pool = require_db(pool)?;

    // Look up linked invariants.
    let invariants: Vec<Invariant> = sqlx::query_as(
        "SELECT i.id, i.name, i.description, i.kind, i.command, i.args, \
         i.expected_exit_code, i.threshold, i.scope, i.created_at \
         FROM invariants i \
         INNER JOIN task_invariants ti ON ti.invariant_id = i.id \
         WHERE ti.task_id = $1 \
         ORDER BY i.name",
    )
    .bind(claims.task_id)
    .fetch_all(pool)
    .await
    .context("failed to query invariants for task")?;

    if invariants.is_empty() {
        println!("No invariants linked to this task. Nothing to check.");
        return Ok(());
    }

    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let mut results = Vec::with_capacity(invariants.len());
    let mut all_passed = true;

    for inv in &invariants {
        let result = run_invariant_check(inv, &cwd)?;
        if !result.passed {
            all_passed = false;
        }
        results.push(result);
    }

    // Print results.
    println!("## Invariant check results");
    println!();

    for result in &results {
        let status_label = if result.passed { "PASS" } else { "FAIL" };
        println!(
            "- [{}] **{}** (exit {}, {}ms)",
            status_label, result.name, result.exit_code, result.duration_ms
        );
        if !result.passed {
            if !result.stderr_snippet.is_empty() {
                println!("  stderr: {}", result.stderr_snippet);
            }
            if !result.stdout_snippet.is_empty() {
                println!("  stdout: {}", result.stdout_snippet);
            }
        }
    }
    println!();

    // Record results as agent events (progress).
    let check_payload = serde_json::json!({
        "type": "invariant_check",
        "all_passed": all_passed,
        "results": results.iter().map(|r| {
            serde_json::json!({
                "name": r.name,
                "passed": r.passed,
                "exit_code": r.exit_code,
                "duration_ms": r.duration_ms,
            })
        }).collect::<Vec<_>>(),
    });

    // Best-effort: record the check event. If the DB insert fails we still
    // report the invariant results.
    let _ = sqlx::query(
        "INSERT INTO agent_events (task_id, attempt, event_type, payload) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(claims.task_id)
    .bind(claims.attempt as i32)
    .bind("invariant_check")
    .bind(&check_payload)
    .execute(pool)
    .await;

    if all_passed {
        println!("All invariants passed.");
        Ok(())
    } else {
        bail!("One or more invariants failed.");
    }
}

/// Execute a single invariant in the given working directory.
fn run_invariant_check(
    inv: &Invariant,
    cwd: &std::path::Path,
) -> Result<InvariantCheckResult> {
    let start = Instant::now();

    let output = std::process::Command::new(&inv.command)
        .args(&inv.args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("failed to execute invariant '{}': {}", inv.name, inv.command))?;

    let duration_ms = start.elapsed().as_millis() as u64;
    let exit_code = output.status.code().unwrap_or(-1);
    let passed = exit_code == inv.expected_exit_code;

    let stdout_full = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr_full = String::from_utf8_lossy(&output.stderr).to_string();

    // Truncate to a reasonable snippet length.
    let snippet_max = 500;
    let stdout_snippet = truncate_string(&stdout_full, snippet_max);
    let stderr_snippet = truncate_string(&stderr_full, snippet_max);

    Ok(InvariantCheckResult {
        name: inv.name.clone(),
        passed,
        exit_code,
        stdout_snippet,
        stderr_snippet,
        duration_ms,
    })
}

/// Truncate a string to at most `max_len` characters, appending "..." if
/// truncated.
fn truncate_string(s: &str, max_len: usize) -> String {
    let trimmed = s.trim();
    if trimmed.len() <= max_len {
        trimmed.to_string()
    } else {
        format!("{}...", &trimmed[..max_len])
    }
}

// -----------------------------------------------------------------------
// gator progress
// -----------------------------------------------------------------------

/// `gator progress "message"` -- record a progress event.
async fn cmd_progress(
    claims: &TokenClaims,
    pool: Option<&PgPool>,
    message: &str,
) -> Result<()> {
    let pool = require_db(pool)?;

    let payload = serde_json::json!({
        "message": message,
    });

    sqlx::query(
        "INSERT INTO agent_events (task_id, attempt, event_type, payload) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(claims.task_id)
    .bind(claims.attempt as i32)
    .bind("progress")
    .bind(&payload)
    .execute(pool)
    .await
    .context("failed to record progress event")?;

    println!("Progress recorded for task {}.", claims.task_id);
    Ok(())
}

// -----------------------------------------------------------------------
// gator done
// -----------------------------------------------------------------------

/// `gator done` -- signal task completion.
///
/// Records a done_signal event but does NOT change the task status.
/// That is gator's (the orchestrator's) job.
async fn cmd_done(claims: &TokenClaims, pool: Option<&PgPool>) -> Result<()> {
    let pool = require_db(pool)?;

    let payload = serde_json::json!({
        "task_id": claims.task_id.to_string(),
        "attempt": claims.attempt,
    });

    sqlx::query(
        "INSERT INTO agent_events (task_id, attempt, event_type, payload) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(claims.task_id)
    .bind(claims.attempt as i32)
    .bind("done_signal")
    .bind(&payload)
    .execute(pool)
    .await
    .context("failed to record done signal")?;

    println!("Completion signaled. Gator will now run gate checks.");
    Ok(())
}

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

/// Require that a database pool is available.
fn require_db(pool: Option<&PgPool>) -> Result<&PgPool> {
    pool.ok_or_else(|| {
        anyhow::anyhow!(
            "database connection required but not available; \
             set GATOR_DATABASE_URL or run `gator init` first"
        )
    })
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use gator_core::token::guard::{self, AGENT_TOKEN_ENV};
    use gator_core::token::{TokenConfig, generate_token};
    use std::sync::Mutex;
    use uuid::Uuid;

    use crate::Commands;

    fn test_config() -> TokenConfig {
        TokenConfig::new(b"agent-mode-test-secret".to_vec())
    }

    // Mutex to serialize tests that modify environment variables.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn operator_commands_rejected_when_token_set() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let config = test_config();
        let task_id = Uuid::new_v4();
        let token = generate_token(&config, task_id, 0);

        // SAFETY: serialized by mutex, test-only code.
        unsafe { std::env::set_var(AGENT_TOKEN_ENV, &token) };

        // Verify the guard detects agent mode.
        let result = guard::require_operator_mode();
        assert!(result.is_err(), "operator mode should be blocked when token is set");

        unsafe { std::env::remove_var(AGENT_TOKEN_ENV) };
    }

    #[test]
    fn operator_commands_allowed_when_no_token() {
        let _lock = ENV_MUTEX.lock().unwrap();

        // SAFETY: serialized by mutex, test-only code.
        unsafe { std::env::remove_var(AGENT_TOKEN_ENV) };

        let result = guard::require_operator_mode();
        assert!(result.is_ok(), "operator mode should be allowed when no token");
    }

    #[test]
    fn invalid_token_rejected() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let config = test_config();

        // SAFETY: serialized by mutex, test-only code.
        unsafe { std::env::set_var(AGENT_TOKEN_ENV, "gator_at_bogus_token_value") };

        let result = guard::require_agent_mode(&config);
        assert!(result.is_err(), "invalid token should be rejected");

        unsafe { std::env::remove_var(AGENT_TOKEN_ENV) };
    }

    #[test]
    fn valid_token_accepted() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let config = test_config();
        let task_id = Uuid::new_v4();
        let attempt = 1;
        let token = generate_token(&config, task_id, attempt);

        // SAFETY: serialized by mutex, test-only code.
        unsafe { std::env::set_var(AGENT_TOKEN_ENV, &token) };
        unsafe { std::env::set_var("GATOR_TOKEN_SECRET", "agent-mode-test-secret") };

        let result = guard::require_agent_mode(&config);
        assert!(result.is_ok(), "valid token should be accepted");

        let claims = result.unwrap();
        assert_eq!(claims.task_id, task_id);
        assert_eq!(claims.attempt, attempt);

        unsafe { std::env::remove_var(AGENT_TOKEN_ENV) };
        unsafe { std::env::remove_var("GATOR_TOKEN_SECRET") };
    }

    #[test]
    fn tampered_token_rejected() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let config = test_config();
        let task_id = Uuid::new_v4();
        let mut token = generate_token(&config, task_id, 1);

        // Tamper with the last character.
        let last = token.pop().unwrap();
        token.push(if last == 'a' { 'b' } else { 'a' });

        // SAFETY: serialized by mutex, test-only code.
        unsafe { std::env::set_var(AGENT_TOKEN_ENV, &token) };

        let result = guard::require_agent_mode(&config);
        assert!(result.is_err(), "tampered token should be rejected");

        unsafe { std::env::remove_var(AGENT_TOKEN_ENV) };
    }

    #[tokio::test]
    async fn agent_mode_rejects_init_command() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let config = test_config();
        let task_id = Uuid::new_v4();
        let token = generate_token(&config, task_id, 0);

        // SAFETY: serialized by mutex, test-only code.
        unsafe { std::env::set_var(AGENT_TOKEN_ENV, &token) };
        unsafe { std::env::set_var("GATOR_TOKEN_SECRET", "agent-mode-test-secret") };

        let result = super::run_agent_mode(
            Commands::Init {
                db_url: "postgresql://localhost:5432/gator".into(),
                force: false,
            },
            None,
        )
        .await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("not available in agent mode"),
            "expected agent mode rejection message, got: {err_msg}"
        );

        unsafe { std::env::remove_var(AGENT_TOKEN_ENV) };
        unsafe { std::env::remove_var("GATOR_TOKEN_SECRET") };
    }

    #[tokio::test]
    async fn agent_mode_rejects_plan_command() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let config = test_config();
        let task_id = Uuid::new_v4();
        let token = generate_token(&config, task_id, 0);

        // SAFETY: serialized by mutex, test-only code.
        unsafe { std::env::set_var(AGENT_TOKEN_ENV, &token) };
        unsafe { std::env::set_var("GATOR_TOKEN_SECRET", "agent-mode-test-secret") };

        let result = super::run_agent_mode(
            Commands::Plan {
                command: crate::PlanCommands::Show { plan_id: None },
            },
            None,
        )
        .await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("not available in agent mode"),
            "expected agent mode rejection message, got: {err_msg}"
        );

        unsafe { std::env::remove_var(AGENT_TOKEN_ENV) };
        unsafe { std::env::remove_var("GATOR_TOKEN_SECRET") };
    }

    #[tokio::test]
    async fn agent_mode_rejects_invariant_command() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let config = test_config();
        let task_id = Uuid::new_v4();
        let token = generate_token(&config, task_id, 0);

        // SAFETY: serialized by mutex, test-only code.
        unsafe { std::env::set_var(AGENT_TOKEN_ENV, &token) };
        unsafe { std::env::set_var("GATOR_TOKEN_SECRET", "agent-mode-test-secret") };

        let result = super::run_agent_mode(
            Commands::Invariant {
                command: crate::InvariantCommands::List { verbose: false },
            },
            None,
        )
        .await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("not available in agent mode"),
            "expected agent mode rejection message, got: {err_msg}"
        );

        unsafe { std::env::remove_var(AGENT_TOKEN_ENV) };
        unsafe { std::env::remove_var("GATOR_TOKEN_SECRET") };
    }

    #[tokio::test]
    async fn agent_mode_task_requires_db() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let config = test_config();
        let task_id = Uuid::new_v4();
        let token = generate_token(&config, task_id, 0);

        // SAFETY: serialized by mutex, test-only code.
        unsafe { std::env::set_var(AGENT_TOKEN_ENV, &token) };
        unsafe { std::env::set_var("GATOR_TOKEN_SECRET", "agent-mode-test-secret") };

        // Without a DB pool, the command should fail with a helpful message.
        let result = super::run_agent_mode(Commands::Task, None).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("database connection required"),
            "expected DB required message, got: {err_msg}"
        );

        unsafe { std::env::remove_var(AGENT_TOKEN_ENV) };
        unsafe { std::env::remove_var("GATOR_TOKEN_SECRET") };
    }

    #[tokio::test]
    async fn agent_mode_check_requires_db() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let config = test_config();
        let task_id = Uuid::new_v4();
        let token = generate_token(&config, task_id, 0);

        // SAFETY: serialized by mutex, test-only code.
        unsafe { std::env::set_var(AGENT_TOKEN_ENV, &token) };
        unsafe { std::env::set_var("GATOR_TOKEN_SECRET", "agent-mode-test-secret") };

        let result = super::run_agent_mode(Commands::Check, None).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("database connection required"),
            "expected DB required message, got: {err_msg}"
        );

        unsafe { std::env::remove_var(AGENT_TOKEN_ENV) };
        unsafe { std::env::remove_var("GATOR_TOKEN_SECRET") };
    }

    #[tokio::test]
    async fn agent_mode_progress_requires_db() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let config = test_config();
        let task_id = Uuid::new_v4();
        let token = generate_token(&config, task_id, 0);

        // SAFETY: serialized by mutex, test-only code.
        unsafe { std::env::set_var(AGENT_TOKEN_ENV, &token) };
        unsafe { std::env::set_var("GATOR_TOKEN_SECRET", "agent-mode-test-secret") };

        let result = super::run_agent_mode(
            Commands::Progress {
                message: "working on it".to_string(),
            },
            None,
        )
        .await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("database connection required"),
            "expected DB required message, got: {err_msg}"
        );

        unsafe { std::env::remove_var(AGENT_TOKEN_ENV) };
        unsafe { std::env::remove_var("GATOR_TOKEN_SECRET") };
    }

    #[tokio::test]
    async fn agent_mode_done_requires_db() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let config = test_config();
        let task_id = Uuid::new_v4();
        let token = generate_token(&config, task_id, 0);

        // SAFETY: serialized by mutex, test-only code.
        unsafe { std::env::set_var(AGENT_TOKEN_ENV, &token) };
        unsafe { std::env::set_var("GATOR_TOKEN_SECRET", "agent-mode-test-secret") };

        let result = super::run_agent_mode(Commands::Done, None).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("database connection required"),
            "expected DB required message, got: {err_msg}"
        );

        unsafe { std::env::remove_var(AGENT_TOKEN_ENV) };
        unsafe { std::env::remove_var("GATOR_TOKEN_SECRET") };
    }

    #[test]
    fn truncate_string_within_limit() {
        let s = "hello world";
        assert_eq!(super::truncate_string(s, 100), "hello world");
    }

    #[test]
    fn truncate_string_at_limit() {
        let s = "abcde";
        assert_eq!(super::truncate_string(s, 5), "abcde");
    }

    #[test]
    fn truncate_string_over_limit() {
        let s = "abcdefghij";
        assert_eq!(super::truncate_string(s, 5), "abcde...");
    }

    #[test]
    fn truncate_string_trims_whitespace() {
        let s = "  hello  ";
        assert_eq!(super::truncate_string(s, 100), "hello");
    }
}
