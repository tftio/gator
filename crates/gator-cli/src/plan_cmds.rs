//! Operator-mode CLI handlers for `gator plan` subcommands.
//!
//! Implements:
//! - `gator plan init <name>`       -- scaffold a plan TOML with project-aware defaults
//! - `gator plan generate [DESC]`   -- generate a plan TOML (interactive or orchestrated)
//! - `gator plan validate <file>`   -- validate a plan TOML file
//! - `gator plan create <file>`     -- create a plan from a TOML file
//! - `gator plan show [plan-id]`    -- show plan details or list all plans
//! - `gator plan approve <plan-id>` -- transition a plan from draft to approved
//! - `gator plan export <plan-id>`  -- export a plan as TOML

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use gator_core::harness::{ClaudeCodeAdapter, HarnessRegistry};
use gator_core::isolation;
use gator_core::orchestrator::{OrchestratorConfig, OrchestratorResult, run_orchestrator};
use gator_core::plan::{
    GenerateContext, build_meta_plan, build_system_prompt, create_plan_from_toml, detect_context,
    get_plan_with_tasks, invariants_from_presets, materialize_plan, parse_plan_toml,
    validate_generated_plan,
};
use gator_core::presets;
use gator_core::token::TokenConfig;
use gator_db::models::{InvariantKind, InvariantScope};
use gator_db::queries::{
    gate_results, invariants as inv_queries, plans as plan_queries, tasks as task_queries,
};

use crate::PlanCommands;

// -----------------------------------------------------------------------
// Public entry point
// -----------------------------------------------------------------------

/// Dispatch a `PlanCommands` variant to the appropriate handler.
///
/// `pool` is `None` for commands that don't need a database (e.g.
/// `plan init --no-register`, `plan validate`, interactive `plan generate`).
///
/// `token_config` is only needed for the orchestrated `plan generate` path
/// (one-shot with a description).
pub async fn run_plan_command(
    command: PlanCommands,
    pool: Option<&PgPool>,
    token_config: Option<&TokenConfig>,
) -> Result<()> {
    match command {
        PlanCommands::Init {
            name,
            project_type,
            no_register,
            output,
        } => {
            cmd_plan_init(
                pool,
                &name,
                project_type.as_deref(),
                no_register,
                output.as_deref(),
            )
            .await
        }
        PlanCommands::Generate {
            description,
            file,
            output,
            base_branch,
            no_validate,
            dry_run,
            gate,
        } => {
            cmd_plan_generate(
                pool,
                token_config,
                description,
                file,
                &output,
                base_branch.as_deref(),
                no_validate,
                dry_run,
                &gate,
            )
            .await
        }
        PlanCommands::Validate { file } => cmd_plan_validate(&file),
        PlanCommands::Create { file } => {
            let pool = pool.context("database connection required for plan create")?;
            cmd_create(pool, &file).await
        }
        PlanCommands::Show { plan_id } => {
            let pool = pool.context("database connection required for plan show")?;
            match plan_id {
                Some(id) => cmd_show_one(pool, &id).await,
                None => cmd_show_all(pool).await,
            }
        }
        PlanCommands::Approve { plan_id } => {
            let pool = pool.context("database connection required for plan approve")?;
            cmd_approve(pool, &plan_id).await
        }
        PlanCommands::Export { plan_id, output } => {
            let pool = pool.context("database connection required for plan export")?;
            cmd_export(pool, &plan_id, output.as_deref()).await
        }
    }
}

// -----------------------------------------------------------------------
// gator plan init <name>
// -----------------------------------------------------------------------

/// Scaffold a new plan TOML with project-aware defaults.
///
/// Detects the project type and base branch, optionally registers preset
/// invariants in the database, and writes a starter plan TOML file.
async fn cmd_plan_init(
    pool: Option<&PgPool>,
    name: &str,
    project_type_override: Option<&str>,
    no_register: bool,
    output_override: Option<&str>,
) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;

    // 1. Resolve project type.
    let project_type = match project_type_override {
        Some(pt) => {
            // Validate it is a known type.
            let known = presets::available_project_types();
            if !known.contains(&pt.to_string()) {
                bail!(
                    "unknown project type {:?}; available types: {}",
                    pt,
                    known.join(", ")
                );
            }
            pt.to_string()
        }
        None => match presets::detect_project_type(&cwd) {
            Some(pt) => {
                println!("Detected project type: {}", pt);
                pt
            }
            None => {
                bail!(
                    "could not detect project type in {}.\n\
                     Use --project-type to specify one of: {}",
                    cwd.display(),
                    presets::available_project_types().join(", ")
                );
            }
        },
    };

    // 2. Detect base branch.
    let base_branch = presets::detect_base_branch(&cwd);

    // 3. Get matching presets.
    let matching_presets = presets::presets_for_project_type(&project_type);

    // 4. Register invariants in DB (unless --no-register).
    if !no_register {
        if let Some(pool) = pool {
            let (registered, skipped) = register_presets(pool, &matching_presets).await?;
            if !registered.is_empty() {
                println!(
                    "Registered {} invariant(s): {}",
                    registered.len(),
                    registered.join(", ")
                );
            }
            if !skipped.is_empty() {
                println!(
                    "Skipped {} invariant(s) (already exist): {}",
                    skipped.len(),
                    skipped.join(", ")
                );
            }
        } else {
            println!("No database connection; skipping invariant registration.");
            println!("Run `gator invariant presets install` later to register them.");
        }
    }

    // 5. Generate the plan TOML content.
    let invariant_names: Vec<&str> = matching_presets.iter().map(|p| p.name.as_str()).collect();
    let toml_content = generate_plan_toml(name, &base_branch, &invariant_names);

    // 6. Write to file.
    let output_path = match output_override {
        Some(p) => p.to_string(),
        None => format!("{name}.toml"),
    };

    if Path::new(&output_path).exists() {
        bail!(
            "file {:?} already exists. Use --output to specify a different path.",
            output_path
        );
    }

    std::fs::write(&output_path, &toml_content)
        .with_context(|| format!("failed to write {}", output_path))?;

    println!();
    println!("Plan scaffolded: {}", output_path);
    println!("  Name:         {}", name);
    println!("  Base branch:  {}", base_branch);
    println!("  Project type: {}", project_type);
    println!(
        "  Invariants:   {}",
        if invariant_names.is_empty() {
            "(none)".to_string()
        } else {
            invariant_names.join(", ")
        }
    );
    println!();
    println!("Next steps:");
    println!("  1. Edit {} to describe your tasks", output_path);
    println!(
        "  2. Run `gator plan create {}` to load it into the database",
        output_path
    );
    println!("  3. Run `gator plan approve <plan-id>` to approve it");
    println!("  4. Run `gator dispatch <plan-id>` to start execution");

    Ok(())
}

/// Register invariant presets in the database, skipping any that already exist.
///
/// Returns `(registered, skipped)` name lists.
async fn register_presets(
    pool: &PgPool,
    preset_list: &[presets::InvariantPreset],
) -> Result<(Vec<String>, Vec<String>)> {
    let mut registered = vec![];
    let mut skipped = vec![];

    for preset in preset_list {
        let existing = inv_queries::get_invariant_by_name(pool, &preset.name).await?;
        if existing.is_some() {
            skipped.push(preset.name.clone());
            continue;
        }

        let kind: InvariantKind = preset.kind.parse().map_err(|_| {
            anyhow::anyhow!(
                "preset {:?} has invalid kind {:?}",
                preset.name,
                preset.kind
            )
        })?;

        let new = inv_queries::NewInvariant {
            name: &preset.name,
            description: Some(&preset.description),
            kind,
            command: &preset.command,
            args: &preset.args,
            expected_exit_code: 0,
            threshold: None,
            scope: InvariantScope::Project,
            timeout_secs: 300,
        };

        inv_queries::insert_invariant(pool, &new).await?;
        registered.push(preset.name.clone());
    }

    Ok((registered, skipped))
}

// -----------------------------------------------------------------------
// gator plan validate <file>
// -----------------------------------------------------------------------

/// Validate a plan TOML file: parse and check structure.
///
/// Exits 0 on success, non-zero on failure.
fn cmd_plan_validate(file: &str) -> Result<()> {
    match validate_generated_plan(file) {
        Ok(plan) => {
            println!("Valid. {} task(s).", plan.tasks.len());
            Ok(())
        }
        Err(e) => {
            eprintln!("Validation failed: {e}");
            std::process::exit(1);
        }
    }
}

// -----------------------------------------------------------------------
// gator plan generate -- helpers
// -----------------------------------------------------------------------

/// Ensure plan-generation invariants exist in the database (idempotent).
async fn ensure_plan_gen_invariants(pool: &PgPool) -> Result<()> {
    let invariants = [
        inv_queries::NewInvariant {
            name: "_gator_plan_file_exists",
            description: Some("Check that plan.toml was created"),
            kind: InvariantKind::Custom,
            command: "test",
            args: &["-f".into(), "plan.toml".into()],
            expected_exit_code: 0,
            threshold: None,
            scope: InvariantScope::Global,
            timeout_secs: 10,
        },
        inv_queries::NewInvariant {
            name: "_gator_plan_validates",
            description: Some("Validate plan.toml parses and has valid structure"),
            kind: InvariantKind::Custom,
            command: "gator",
            args: &["plan".into(), "validate".into(), "plan.toml".into()],
            expected_exit_code: 0,
            threshold: None,
            scope: InvariantScope::Global,
            timeout_secs: 30,
        },
    ];
    for inv in &invariants {
        if inv_queries::get_invariant_by_name(pool, inv.name)
            .await?
            .is_none()
        {
            inv_queries::insert_invariant(pool, inv).await?;
        }
    }
    Ok(())
}

/// Generate plan TOML content with comments showing optional fields.
///
/// The output is hand-built (not `toml::to_string`) to include comments,
/// which the TOML serializer cannot produce. This is intentional -- the
/// generated file is a template for human editing.
fn generate_plan_toml(name: &str, base_branch: &str, invariant_names: &[&str]) -> String {
    let mut out = String::new();

    // [plan] section
    out.push_str("[plan]\n");
    out.push_str(&format!("name = {:?}\n", name));
    out.push_str(&format!("base_branch = {:?}\n", base_branch));
    out.push_str("# token_budget = 500000\n");
    out.push_str("# isolation = \"worktree\"\n");
    out.push_str("# container_image = \"gator-agent:latest\"\n");

    // [[tasks]] stub
    out.push_str("\n[[tasks]]\n");
    out.push_str("name = \"task-1\"\n");
    out.push_str("description = \"\"\"\n");
    out.push_str("Describe what the agent should do.\n");
    out.push_str("\"\"\"\n");
    out.push_str("scope = \"narrow\"\n");
    out.push_str("gate = \"auto\"\n");

    // Invariants array
    if invariant_names.is_empty() {
        out.push_str("invariants = []\n");
    } else {
        let quoted: Vec<String> = invariant_names.iter().map(|n| format!("{:?}", n)).collect();
        out.push_str(&format!("invariants = [{}]\n", quoted.join(", ")));
    }

    out.push_str("# depends_on = []\n");
    out.push_str("# retry_max = 3\n");
    out.push_str("# harness = \"claude-code\"\n");

    out
}

// -----------------------------------------------------------------------
// gator plan generate [DESCRIPTION]
// -----------------------------------------------------------------------

/// Generate a plan TOML by spawning Claude Code with assembled context.
///
/// Two modes:
/// - **Interactive** (no description): spawns Claude Code directly with inherited I/O.
/// - **One-shot** (description provided): creates a meta-plan in the database, runs
///   the orchestrator, and validates the output with invariant gates.
#[allow(clippy::too_many_arguments)]
async fn cmd_plan_generate(
    pool: Option<&PgPool>,
    token_config: Option<&TokenConfig>,
    description: Option<String>,
    file: Option<String>,
    output: &str,
    base_branch_override: Option<&str>,
    no_validate: bool,
    dry_run: bool,
    gate: &str,
) -> Result<()> {
    // 1. Check output file does not already exist.
    if Path::new(output).exists() {
        bail!(
            "output file {:?} already exists. Remove it or use --output to specify a different path.",
            output
        );
    }

    // 2. Resolve description from --file (mutually exclusive with positional arg).
    let desc = match (&description, &file) {
        (Some(_), Some(_)) => bail!("cannot specify both a description argument and --file"),
        (_, Some(path)) => {
            let content = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read description file: {path}"))?;
            let trimmed = content.trim().to_string();
            if trimmed.is_empty() {
                bail!("description file {:?} is empty", path);
            }
            Some(trimmed)
        }
        (Some(d), None) => Some(d.clone()),
        (None, None) => None,
    };

    // 3. Detect context.
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let (base_branch, project_type) = detect_context(&cwd, base_branch_override);

    // 4. Load invariant presets.
    let invariants = invariants_from_presets(project_type.as_deref());

    // 5. Build system prompt. For the orchestrated path the agent writes to
    //    "plan.toml" (relative to its worktree), not the user's --output path.
    let prompt_output = if desc.is_some() { "plan.toml" } else { output };
    let ctx = GenerateContext {
        base_branch: base_branch.clone(),
        project_type: project_type.clone(),
        invariants,
        output_path: prompt_output.to_string(),
    };
    let system_prompt = build_system_prompt(&ctx);

    // 6. Dry-run: print prompt and exit.
    if dry_run {
        println!("{system_prompt}");
        return Ok(());
    }

    // 7. Print context summary.
    let mode = if desc.is_some() {
        "one-shot (orchestrated)"
    } else {
        "interactive"
    };
    println!("Generating plan...");
    println!("  Base branch:  {}", base_branch);
    println!(
        "  Project type: {}",
        project_type.as_deref().unwrap_or("unknown")
    );
    println!("  Mode:         {mode}");
    println!("  Output:       {output}");
    println!();

    // 8. Interactive mode: spawn Claude Code directly.
    if desc.is_none() {
        println!("Starting Claude Code in interactive mode.");
        println!("Describe your feature and Claude will explore the codebase,");
        println!("ask clarifying questions, then write the plan TOML.");
        println!();

        return cmd_plan_generate_interactive(&cwd, &system_prompt, output, no_validate);
    }

    // 9. One-shot mode: orchestrated pipeline.
    let pool = pool.context(
        "one-shot plan generate requires a database (run `gator init` and `gator db-init`)",
    )?;
    let token_config =
        token_config.context("one-shot plan generate requires token config (run `gator init`)")?;

    // a. Ensure plan-generation invariants exist.
    ensure_plan_gen_invariants(pool).await?;

    // b. Build meta-plan.
    let meta_plan = build_meta_plan(&system_prompt, &base_branch, gate);

    // c. Create and approve.
    let project_path = cwd.to_string_lossy();
    let plan = create_plan_from_toml(pool, &meta_plan, &project_path).await?;
    let plan = plan_queries::approve_plan(pool, plan.id).await?;

    println!("  Meta-plan:    {} ({})", plan.name, plan.id);
    println!("  Gate policy:  {gate}");
    println!();

    // d. Set up harness, isolation, orchestrator.
    let mut registry = HarnessRegistry::new();
    registry.register(ClaudeCodeAdapter::new());
    let registry = Arc::new(registry);

    let isolation_backend = isolation::create_isolation("worktree", &cwd, None)?;

    let config = OrchestratorConfig {
        max_agents: 1,
        task_timeout: Duration::from_secs(1800),
    };

    // e. Graceful shutdown handler.
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    let got_first_signal = Arc::new(AtomicBool::new(false));
    let got_first_clone = Arc::clone(&got_first_signal);

    tokio::spawn(async move {
        loop {
            tokio::signal::ctrl_c().await.ok();
            if got_first_clone.swap(true, Ordering::SeqCst) {
                eprintln!("\nForce exit.");
                std::process::exit(130);
            }
            eprintln!("\nShutting down gracefully (Ctrl+C again to force)...");
            cancel_clone.cancel();
        }
    });

    // f. Run orchestrator.
    let result = run_orchestrator(
        pool,
        plan.id,
        &registry,
        &isolation_backend,
        token_config,
        &config,
        cancel,
    )
    .await?;

    // g. Handle result.
    handle_generate_result(pool, plan.id, &result, output).await
}

/// Interactive mode: spawn Claude Code with inherited terminal I/O.
fn cmd_plan_generate_interactive(
    cwd: &Path,
    system_prompt: &str,
    output: &str,
    no_validate: bool,
) -> Result<()> {
    let mut cmd = std::process::Command::new("claude");

    cmd.arg("--append-system-prompt")
        .arg(system_prompt)
        .arg("--disallowedTools")
        .arg("EnterPlanMode")
        .arg("--disable-slash-commands");

    cmd.current_dir(cwd)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    let status = cmd
        .status()
        .context("failed to spawn claude -- is it installed and on PATH?")?;

    if !status.success() {
        let code = status.code().unwrap_or(-1);
        bail!("claude exited with status {code}");
    }

    if no_validate {
        if !Path::new(output).exists() {
            println!("Warning: output file {:?} was not created.", output);
        } else {
            println!("Plan written to {output} (validation skipped).");
        }
        return Ok(());
    }

    print_validation_result(output)
}

/// Validate and print results for a generated plan file.
fn print_validation_result(output: &str) -> Result<()> {
    match validate_generated_plan(output) {
        Ok(plan) => {
            let task_count = plan.tasks.len();
            let dep_edges: usize = plan.tasks.iter().map(|t| t.depends_on.len()).sum();
            let root_tasks = plan
                .tasks
                .iter()
                .filter(|t| t.depends_on.is_empty())
                .count();

            println!();
            println!("Plan validated successfully.");
            println!("  Name:       {}", plan.plan.name);
            println!("  Tasks:      {task_count}");
            println!("  Dep edges:  {dep_edges}");
            println!("  Root tasks: {root_tasks}");
            println!();
            println!("Next: gator plan create {output}");
            Ok(())
        }
        Err(e) => {
            eprintln!();
            eprintln!("Plan validation failed: {e}");
            eprintln!();
            eprintln!("You can:");
            eprintln!("  1. Edit {output} manually to fix the issue");
            eprintln!("  2. Remove {output} and re-run `gator plan generate`");
            std::process::exit(1);
        }
    }
}

/// Handle the orchestrator result after a one-shot plan generation run.
async fn handle_generate_result(
    pool: &PgPool,
    plan_id: Uuid,
    result: &OrchestratorResult,
    output: &str,
) -> Result<()> {
    // Fetch the single task from the meta-plan.
    let tasks = task_queries::list_tasks_for_plan(pool, plan_id).await?;
    let task = tasks
        .first()
        .context("meta-plan has no tasks (unexpected)")?;

    match result {
        OrchestratorResult::Completed => {
            // Auto gate: all invariants passed.
            copy_plan_from_worktree(task.worktree_path.as_deref(), output)?;
            println!("Plan generated and validated successfully.");
            print_validation_result(output)?;
            println!();
            println!("Audit: gator status {plan_id}");
            Ok(())
        }
        OrchestratorResult::HumanRequired { .. } => {
            // Fetch gate results to check invariant outcomes.
            let gate_results = gate_results::get_latest_gate_results(pool, task.id).await?;
            let all_passed = gate_results.iter().all(|r| r.passed);

            if all_passed {
                copy_plan_from_worktree(task.worktree_path.as_deref(), output)?;
                println!("Plan generated -- invariants passed, awaiting review.");
                print_validation_result(output)?;
                println!();
                println!("Review the generated plan, then:");
                println!("  gator plan create {output}");
                println!();
                println!("Audit: gator status {plan_id}");
                Ok(())
            } else {
                eprintln!("Plan generation failed -- invariant check(s) failed:");
                for r in &gate_results {
                    let status = if r.passed { "PASS" } else { "FAIL" };
                    eprintln!("  [{status}] {}", r.invariant_name);
                    if let Some(ref stderr) = r.stderr {
                        if !stderr.is_empty() {
                            for line in stderr.lines().take(5) {
                                eprintln!("    {line}");
                            }
                        }
                    }
                }
                eprintln!();
                eprintln!("Audit: gator status {plan_id}");
                std::process::exit(1);
            }
        }
        OrchestratorResult::Failed { failed_tasks } => {
            eprintln!("Plan generation failed after all retries.");
            for t in failed_tasks {
                eprintln!("  - {t}");
            }
            eprintln!();
            eprintln!("Audit: gator status {plan_id}");
            std::process::exit(1);
        }
        OrchestratorResult::BudgetExceeded { used, budget } => {
            eprintln!("Plan generation stopped: token budget exceeded ({used}/{budget}).");
            std::process::exit(3);
        }
        OrchestratorResult::Interrupted => {
            eprintln!("Plan generation interrupted.");
            eprintln!("Audit: gator status {plan_id}");
            std::process::exit(130);
        }
    }
}

/// Copy plan.toml from the task worktree to the user's output path.
fn copy_plan_from_worktree(worktree_path: Option<&str>, output: &str) -> Result<()> {
    let wt = worktree_path.context("task has no worktree path")?;
    let src = Path::new(wt).join("plan.toml");
    if !src.exists() {
        bail!("plan.toml not found in worktree at {}", src.display());
    }
    std::fs::copy(&src, output)
        .with_context(|| format!("failed to copy {} to {}", src.display(), output))?;
    Ok(())
}

// -----------------------------------------------------------------------
// gator plan create <file>
// -----------------------------------------------------------------------

/// Read a plan.toml from disk, parse and validate it, insert into the DB,
/// and print a summary.
async fn cmd_create(pool: &PgPool, file_path: &str) -> Result<()> {
    // 1. Read the file.
    let content = std::fs::read_to_string(file_path)
        .with_context(|| format!("failed to read plan file: {}", file_path))?;

    // 2. Parse and validate.
    let plan_toml = parse_plan_toml(&content)
        .with_context(|| format!("failed to parse plan file: {}", file_path))?;

    // 3. Reject if the file already has a plan ID.
    if plan_toml.plan.id.is_some() {
        bail!(
            "plan file {:?} already has an id field.\n\
             This plan has already been created. Use `gator plan show` to inspect it,\n\
             or remove the id field to re-create.",
            file_path
        );
    }

    // 4. Determine the project path (current working directory).
    let project_path = std::env::current_dir()
        .context("failed to get current directory")?
        .to_string_lossy()
        .to_string();

    // 5. Insert into DB.
    let plan = create_plan_from_toml(pool, &plan_toml, &project_path).await?;

    // 6. Write plan ID back to the TOML file.
    crate::resolve::write_plan_id_to_file(file_path, plan.id).with_context(|| {
        format!(
            "plan created (ID: {}), but failed to update {}",
            plan.id, file_path
        )
    })?;

    // 7. Count dependency edges.
    let dep_edges = task_queries::count_dependency_edges(pool, plan.id).await?;

    // 8. Print summary.
    println!("Plan created successfully.");
    println!();
    println!("  Plan ID:          {}", plan.id);
    println!("  Name:             {}", plan.name);
    println!("  Status:           {}", plan.status);
    println!("  Tasks:            {}", plan_toml.tasks.len());
    println!("  Dependency edges: {}", dep_edges);
    println!("  Written to:       {}", file_path);

    Ok(())
}

// -----------------------------------------------------------------------
// gator plan show (list all)
// -----------------------------------------------------------------------

/// List all plans with summary info.
async fn cmd_show_all(pool: &PgPool) -> Result<()> {
    let plans = plan_queries::list_plans(pool).await?;

    if plans.is_empty() {
        println!("No plans found. Use `gator plan create <file>` to create one.");
        return Ok(());
    }

    // Build a map of plan_id -> task count.
    let mut task_counts: HashMap<Uuid, i64> = HashMap::new();
    for plan in &plans {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM tasks WHERE plan_id = $1")
            .bind(plan.id)
            .fetch_one(pool)
            .await
            .context("failed to count tasks")?;
        task_counts.insert(plan.id, row.0);
    }

    // Compute column widths for a clean table.
    // ID is always 36 chars (UUID). Status max is 9 (completed).
    let id_w = 36;
    let name_w = plans.iter().map(|p| p.name.len()).max().unwrap_or(4).max(4);
    let status_w = 9;
    let tasks_w = 5;

    // Header
    println!(
        "{:<id_w$}  {:<name_w$}  {:<status_w$}  {:>tasks_w$}  CREATED",
        "ID", "NAME", "STATUS", "TASKS",
    );

    // Rows
    for plan in &plans {
        let count = task_counts.get(&plan.id).copied().unwrap_or(0);
        let created = plan.created_at.format("%Y-%m-%d %H:%M");
        println!(
            "{:<id_w$}  {:<name_w$}  {:<status_w$}  {:>tasks_w$}  {}",
            plan.id, plan.name, plan.status, count, created,
        );
    }

    Ok(())
}

// -----------------------------------------------------------------------
// gator plan show <plan-id>
// -----------------------------------------------------------------------

/// Show detailed info for a single plan.
async fn cmd_show_one(pool: &PgPool, plan_id_str: &str) -> Result<()> {
    let plan_id = crate::resolve::resolve_plan_id(plan_id_str)?;

    let (plan, tasks) = get_plan_with_tasks(pool, plan_id).await?;

    // Plan header.
    println!("Plan: {}", plan.name);
    println!("  ID:           {}", plan.id);
    println!("  Status:       {}", plan.status);
    println!("  Project:      {}", plan.project_path);
    println!("  Base branch:  {}", plan.base_branch);
    println!(
        "  Created:      {}",
        plan.created_at.format("%Y-%m-%d %H:%M:%S UTC")
    );
    if let Some(approved) = plan.approved_at {
        println!(
            "  Approved:     {}",
            approved.format("%Y-%m-%d %H:%M:%S UTC")
        );
    }
    if let Some(completed) = plan.completed_at {
        println!(
            "  Completed:    {}",
            completed.format("%Y-%m-%d %H:%M:%S UTC")
        );
    }
    println!("  Tasks:        {}", tasks.len());

    if tasks.is_empty() {
        return Ok(());
    }

    println!();
    println!("Tasks:");
    println!();

    for task in &tasks {
        // Get dependencies.
        let dep_names = task_queries::get_task_dependency_names(pool, task.id).await?;

        // Get linked invariants.
        let invariants = inv_queries::get_invariants_for_task(pool, task.id).await?;

        println!("  [{}] {}", task.status, task.name);
        println!("    ID:          {}", task.id);
        println!("    Scope:       {}", task.scope_level);
        println!("    Gate:        {}", task.gate_policy);
        println!("    Retry:       {}/{}", task.attempt, task.retry_max);

        if !dep_names.is_empty() {
            println!("    Depends on:  {}", dep_names.join(", "));
        }

        if !invariants.is_empty() {
            let inv_names: Vec<&str> = invariants.iter().map(|i| i.name.as_str()).collect();
            println!("    Invariants:  {}", inv_names.join(", "));
        }

        // Show description (indented, truncated if very long).
        let desc = task.description.trim();
        if !desc.is_empty() {
            println!("    Description:");
            for line in desc.lines().take(10) {
                println!("      {}", line);
            }
            if desc.lines().count() > 10 {
                println!("      ...(truncated)");
            }
        }

        println!();
    }

    Ok(())
}

// -----------------------------------------------------------------------
// gator plan approve <plan-id>
// -----------------------------------------------------------------------

/// Transition a plan from draft to approved.
///
/// Validates that all tasks have at least one invariant linked before
/// approving.
async fn cmd_approve(pool: &PgPool, plan_id_str: &str) -> Result<()> {
    let plan_id = crate::resolve::resolve_plan_id(plan_id_str)?;

    // Check that all tasks have at least one invariant.
    let tasks_without = plan_queries::count_tasks_without_invariants(pool, plan_id).await?;
    if !tasks_without.is_empty() {
        anyhow::bail!(
            "cannot approve plan: {} task(s) have no invariants linked: {}",
            tasks_without.len(),
            tasks_without.join(", "),
        );
    }

    // Perform the approval transition.
    let plan = plan_queries::approve_plan(pool, plan_id).await?;

    println!("Plan approved.");
    println!();
    println!("  Plan ID:     {}", plan.id);
    println!("  Name:        {}", plan.name);
    println!("  Status:      {}", plan.status);
    if let Some(approved) = plan.approved_at {
        println!(
            "  Approved at: {}",
            approved.format("%Y-%m-%d %H:%M:%S UTC")
        );
    }

    Ok(())
}

// -----------------------------------------------------------------------
// gator plan export <plan-id> [--output <file>]
// -----------------------------------------------------------------------

/// Materialize a plan from the database as TOML and write to a file or stdout.
async fn cmd_export(pool: &PgPool, plan_id_str: &str, output: Option<&str>) -> Result<()> {
    let plan_id = crate::resolve::resolve_plan_id(plan_id_str)?;

    let toml_content = materialize_plan(pool, plan_id).await?;

    match output {
        Some(path) => {
            std::fs::write(path, &toml_content)
                .with_context(|| format!("failed to write to {}", path))?;
            println!("Plan exported to {}", path);
        }
        None => {
            print!("{}", toml_content);
        }
    }

    Ok(())
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parse_valid_uuid() {
        let id = "550e8400-e29b-41d4-a716-446655440000";
        let parsed: Uuid = id.parse().unwrap();
        assert_eq!(parsed.to_string(), id);
    }

    #[test]
    fn parse_invalid_uuid() {
        let id = "not-a-uuid";
        let result: Result<Uuid, _> = id.parse();
        assert!(result.is_err());
    }

    // -- TOML generation tests --

    #[test]
    fn generate_plan_toml_with_invariants() {
        let content = generate_plan_toml(
            "my-feature",
            "main",
            &["rust_build", "rust_test", "rust_clippy"],
        );

        // Verify it parses as valid TOML.
        let parsed: gator_core::plan::PlanToml =
            toml::from_str(&content).expect("generated TOML should parse");
        assert_eq!(parsed.plan.name, "my-feature");
        assert_eq!(parsed.plan.base_branch, "main");
        assert_eq!(parsed.tasks.len(), 1);
        assert_eq!(parsed.tasks[0].name, "task-1");
        assert_eq!(
            parsed.tasks[0].invariants,
            vec!["rust_build", "rust_test", "rust_clippy"]
        );
    }

    #[test]
    fn generate_plan_toml_no_invariants() {
        let content = generate_plan_toml("empty-plan", "develop", &[]);

        let parsed: gator_core::plan::PlanToml =
            toml::from_str(&content).expect("generated TOML should parse");
        assert_eq!(parsed.plan.name, "empty-plan");
        assert_eq!(parsed.plan.base_branch, "develop");
        assert!(parsed.tasks[0].invariants.is_empty());
    }

    #[test]
    fn generate_plan_toml_contains_comments() {
        let content = generate_plan_toml("test", "main", &[]);
        // Comments should be present for optional fields.
        assert!(content.contains("# token_budget"));
        assert!(content.contains("# isolation"));
        assert!(content.contains("# container_image"));
        assert!(content.contains("# depends_on"));
        assert!(content.contains("# retry_max"));
        assert!(content.contains("# harness"));
    }

    #[test]
    fn generate_plan_toml_special_chars_in_name() {
        let content = generate_plan_toml("add-user-auth", "main", &[]);
        let parsed: gator_core::plan::PlanToml =
            toml::from_str(&content).expect("generated TOML should parse");
        assert_eq!(parsed.plan.name, "add-user-auth");
    }

    // -- CLI parsing tests --

    #[derive(Parser)]
    #[command(name = "gator")]
    struct TestCli {
        #[command(subcommand)]
        command: TestCommands,
    }

    #[derive(clap::Subcommand)]
    enum TestCommands {
        Plan {
            #[command(subcommand)]
            command: PlanCommands,
        },
    }

    #[test]
    fn clap_parses_plan_init() {
        let cli =
            TestCli::try_parse_from(["gator", "plan", "init", "my-feature"]).expect("should parse");
        match cli.command {
            TestCommands::Plan {
                command:
                    PlanCommands::Init {
                        name,
                        project_type,
                        no_register,
                        output,
                    },
            } => {
                assert_eq!(name, "my-feature");
                assert!(project_type.is_none());
                assert!(!no_register);
                assert!(output.is_none());
            }
            _ => panic!("expected Plan Init"),
        }
    }

    #[test]
    fn clap_parses_plan_init_with_options() {
        let cli = TestCli::try_parse_from([
            "gator",
            "plan",
            "init",
            "my-feature",
            "--project-type",
            "rust",
            "--no-register",
            "--output",
            "custom.toml",
        ])
        .expect("should parse");
        match cli.command {
            TestCommands::Plan {
                command:
                    PlanCommands::Init {
                        name,
                        project_type,
                        no_register,
                        output,
                    },
            } => {
                assert_eq!(name, "my-feature");
                assert_eq!(project_type.as_deref(), Some("rust"));
                assert!(no_register);
                assert_eq!(output.as_deref(), Some("custom.toml"));
            }
            _ => panic!("expected Plan Init"),
        }
    }

    #[test]
    fn clap_plan_init_missing_name_fails() {
        let result = TestCli::try_parse_from(["gator", "plan", "init"]);
        assert!(result.is_err(), "missing name should fail");
    }

    // -- plan generate CLI parsing tests --

    #[test]
    fn clap_parses_plan_generate_with_description() {
        let cli = TestCli::try_parse_from(["gator", "plan", "generate", "Add user authentication"])
            .expect("should parse");
        match cli.command {
            TestCommands::Plan {
                command:
                    PlanCommands::Generate {
                        description,
                        file,
                        output,
                        base_branch,
                        no_validate,
                        dry_run,
                        gate,
                    },
            } => {
                assert_eq!(description.as_deref(), Some("Add user authentication"));
                assert!(file.is_none());
                assert_eq!(output, "plan.toml");
                assert!(base_branch.is_none());
                assert!(!no_validate);
                assert!(!dry_run);
                assert_eq!(gate, "human_review");
            }
            _ => panic!("expected Plan Generate"),
        }
    }

    #[test]
    fn clap_parses_plan_generate_interactive() {
        let cli = TestCli::try_parse_from(["gator", "plan", "generate"]).expect("should parse");
        match cli.command {
            TestCommands::Plan {
                command: PlanCommands::Generate { description, .. },
            } => {
                assert!(description.is_none());
            }
            _ => panic!("expected Plan Generate"),
        }
    }

    #[test]
    fn clap_parses_plan_generate_all_options() {
        let cli = TestCli::try_parse_from([
            "gator",
            "plan",
            "generate",
            "Add auth",
            "--file",
            "desc.md",
            "--output",
            "auth.toml",
            "--base-branch",
            "develop",
            "--no-validate",
            "--dry-run",
            "--gate",
            "auto",
        ])
        .expect("should parse");
        match cli.command {
            TestCommands::Plan {
                command:
                    PlanCommands::Generate {
                        description,
                        file,
                        output,
                        base_branch,
                        no_validate,
                        dry_run,
                        gate,
                    },
            } => {
                assert_eq!(description.as_deref(), Some("Add auth"));
                assert_eq!(file.as_deref(), Some("desc.md"));
                assert_eq!(output, "auth.toml");
                assert_eq!(base_branch.as_deref(), Some("develop"));
                assert!(no_validate);
                assert!(dry_run);
                assert_eq!(gate, "auto");
            }
            _ => panic!("expected Plan Generate"),
        }
    }

    #[test]
    fn clap_parses_plan_validate() {
        let cli = TestCli::try_parse_from(["gator", "plan", "validate", "plan.toml"])
            .expect("should parse");
        match cli.command {
            TestCommands::Plan {
                command: PlanCommands::Validate { file },
            } => {
                assert_eq!(file, "plan.toml");
            }
            _ => panic!("expected Plan Validate"),
        }
    }
}
