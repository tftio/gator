//! Plan materialization: reconstruct plan TOML and task markdown from DB state.
//!
//! - [`materialize_plan`] produces a valid `plan.toml` string from the database,
//!   including current task status so readers can see progress.
//! - [`materialize_task`] produces a standalone markdown document for a single
//!   task, suitable for handing to an agent.

use anyhow::{Context, Result};
use sqlx::PgPool;
use uuid::Uuid;

use gator_db::queries::{
    gate_results, invariants as inv_queries, plans as plan_queries, tasks as task_queries,
};

use super::toml_format::PlanToml;

/// Materialize a plan from the database back to `plan.toml` content.
///
/// The output is valid TOML that can be parsed by [`super::parse_plan_toml`].
/// Each task entry includes a `status` comment so readers can see progress,
/// and a `status` field in the TOML itself.
pub async fn materialize_plan(pool: &PgPool, plan_id: Uuid) -> Result<String> {
    let plan = plan_queries::get_plan(pool, plan_id)
        .await?
        .with_context(|| format!("plan {plan_id} not found"))?;

    let tasks = task_queries::list_tasks_for_plan(pool, plan_id).await?;

    // Build a structured representation, then serialize manually so we can
    // include the status field and preserve multiline descriptions properly.
    let mut out = String::new();

    // Plan header.
    out.push_str("[plan]\n");
    out.push_str(&format!("id = {}\n", toml_quote(&plan.id.to_string())));
    out.push_str(&format!("name = {}\n", toml_quote(&plan.name)));
    out.push_str(&format!(
        "base_branch = {}\n",
        toml_quote(&plan.base_branch)
    ));
    out.push_str(&format!(
        "default_harness = {}\n",
        toml_quote(&plan.default_harness)
    ));
    out.push_str(&format!("isolation = {}\n", toml_quote(&plan.isolation)));

    for task in &tasks {
        out.push('\n');
        out.push_str("[[tasks]]\n");
        out.push_str(&format!("name = {}\n", toml_quote(&task.name)));
        out.push_str(&format!(
            "description = {}\n",
            toml_quote(&task.description)
        ));
        out.push_str(&format!(
            "scope = {}\n",
            toml_quote(&task.scope_level.to_string())
        ));
        out.push_str(&format!(
            "gate = {}\n",
            toml_quote(&task.gate_policy.to_string())
        ));
        out.push_str(&format!("retry_max = {}\n", task.retry_max));
        if let Some(ref harness) = task.requested_harness {
            out.push_str(&format!("harness = {}\n", toml_quote(harness)));
        }
        out.push_str(&format!(
            "status = {}\n",
            toml_quote(&task.status.to_string())
        ));

        // Dependencies.
        let dep_names = task_queries::get_task_dependency_names(pool, task.id).await?;
        if !dep_names.is_empty() {
            let dep_strs: Vec<String> = dep_names.iter().map(|n| toml_quote(n)).collect();
            out.push_str(&format!("depends_on = [{}]\n", dep_strs.join(", ")));
        } else {
            out.push_str("depends_on = []\n");
        }

        // Invariants.
        let invariants = inv_queries::get_invariants_for_task(pool, task.id).await?;
        if !invariants.is_empty() {
            let inv_strs: Vec<String> = invariants.iter().map(|i| toml_quote(&i.name)).collect();
            out.push_str(&format!("invariants = [{}]\n", inv_strs.join(", ")));
        } else {
            out.push_str("invariants = []\n");
        }
    }

    Ok(out)
}

/// Materialize a single task as a standalone markdown document.
///
/// The document is designed to be handed to an agent and includes:
/// - Task name and description
/// - Invariant commands (so the agent can run `gator check`)
/// - Scope and gate policy
/// - Dependencies and their current statuses
///
/// It does NOT include plan-level context, other tasks' details, or database
/// identifiers.
pub async fn materialize_task(pool: &PgPool, task_id: Uuid) -> Result<String> {
    let task = task_queries::get_task(pool, task_id)
        .await?
        .with_context(|| format!("task {task_id} not found"))?;

    let mut out = String::new();

    // Title
    out.push_str(&format!("# Task: {}\n\n", task.name));

    // Status
    out.push_str(&format!("**Status:** {}\n\n", task.status));

    // Scope and gate
    out.push_str(&format!("**Scope:** {}  \n", task.scope_level));
    out.push_str(&format!("**Gate policy:** {}\n\n", task.gate_policy));

    // Description
    out.push_str("## Description\n\n");
    out.push_str(task.description.trim());
    out.push_str("\n\n");

    // Dependencies
    let dep_names = task_queries::get_task_dependency_names(pool, task.id).await?;
    if !dep_names.is_empty() {
        out.push_str("## Dependencies\n\n");

        // For each dependency, look up its status.
        for dep_name in &dep_names {
            let dep_status = get_dependency_status_by_name(pool, task.plan_id, dep_name).await?;
            out.push_str(&format!("- **{}**: {}\n", dep_name, dep_status));
        }
        out.push('\n');
    }

    // Invariants
    let invariants = inv_queries::get_invariants_for_task(pool, task.id).await?;
    if !invariants.is_empty() {
        out.push_str("## Invariants\n\n");
        out.push_str("Run `gator check` to verify all invariants pass.\n\n");
        for inv in &invariants {
            let args_str = if inv.args.is_empty() {
                String::new()
            } else {
                format!(" {}", inv.args.join(" "))
            };
            out.push_str(&format!(
                "- **{}**: `{}{}`",
                inv.name, inv.command, args_str
            ));
            if let Some(desc) = &inv.description {
                out.push_str(&format!(" -- {}", desc));
            }
            out.push('\n');
        }
        out.push('\n');
    }

    // Previous Attempt Feedback (retry context)
    if task.attempt > 0 {
        let prev_attempt = task.attempt - 1;
        let prev_results = gate_results::get_gate_results(pool, task.id, prev_attempt).await?;
        let failures: Vec<_> = prev_results.iter().filter(|r| !r.passed).collect();

        if !failures.is_empty() {
            out.push_str("## Previous Attempt Feedback\n\n");
            out.push_str(&format!(
                "Attempt {} failed. The following invariants did not pass:\n\n",
                prev_attempt
            ));

            for failure in &failures {
                let inv_name = match inv_queries::get_invariant(pool, failure.invariant_id).await? {
                    Some(inv) => inv.name,
                    None => format!("unknown ({})", failure.invariant_id),
                };

                let exit_code = failure
                    .exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string());

                let stderr_snippet = failure.stderr.as_deref().unwrap_or("").to_string();
                let stderr_truncated = truncate_feedback_snippet(&stderr_snippet, 2048);

                out.push_str(&format!("### {}\n\n", inv_name));
                out.push_str(&format!("- **Exit code:** {}\n", exit_code));
                if !stderr_truncated.is_empty() {
                    out.push_str("- **Stderr:**\n```\n");
                    out.push_str(&stderr_truncated);
                    out.push_str("\n```\n");
                }
                out.push('\n');
            }
        }
    }

    Ok(out)
}

/// Look up a task's status by name within a plan.
async fn get_dependency_status_by_name(
    pool: &PgPool,
    plan_id: Uuid,
    task_name: &str,
) -> Result<String> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT status::text FROM tasks WHERE plan_id = $1 AND name = $2")
            .bind(plan_id)
            .bind(task_name)
            .fetch_optional(pool)
            .await
            .with_context(|| {
                format!(
                    "failed to look up dependency status for task {:?}",
                    task_name
                )
            })?;

    match row {
        Some((status,)) => Ok(status),
        None => Ok("unknown".to_string()),
    }
}

/// Truncate a string to at most `max_bytes` bytes for feedback snippets,
/// appending "..." if truncated.
fn truncate_feedback_snippet(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = s[..end].to_owned();
    truncated.push_str("...");
    truncated
}

/// Quote a string as a TOML value. Uses triple-quoted strings for multiline
/// values and regular quoted strings otherwise.
fn toml_quote(s: &str) -> String {
    if s.contains('\n') {
        // Use triple-quoted basic strings for multiline values.
        // The leading newline after """ is stripped by TOML parsers.
        format!("\"\"\"\n{}\\\n\"\"\"", s)
    } else {
        // Use a regular quoted string, escaping embedded quotes and backslashes.
        let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{}\"", escaped)
    }
}

/// Parse a materialized plan TOML string into a [`PlanToml`].
///
/// This is a convenience wrapper that strips the `status` field (which is
/// not part of the input format) before parsing. The `TaskToml` struct uses
/// `#[serde(default)]` so the extra `status` field is simply ignored by the
/// TOML deserializer if `deny_unknown_fields` is not set.
pub fn parse_materialized(content: &str) -> Result<PlanToml> {
    let plan: PlanToml =
        toml::from_str(content).context("failed to parse materialized plan TOML")?;
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_quote_simple() {
        assert_eq!(toml_quote("hello"), "\"hello\"");
    }

    #[test]
    fn toml_quote_with_embedded_quotes() {
        assert_eq!(toml_quote("say \"hi\""), r#""say \"hi\"""#);
    }

    #[test]
    fn toml_quote_multiline() {
        let s = "line one\nline two";
        let quoted = toml_quote(s);
        assert!(quoted.starts_with("\"\"\""));
        assert!(quoted.ends_with("\"\"\""));
    }

    #[test]
    fn toml_quote_backslash() {
        assert_eq!(toml_quote("a\\b"), r#""a\\b""#);
    }
}
