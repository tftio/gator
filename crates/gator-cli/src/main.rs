mod agent;
mod cleanup_cmd;
mod config;
mod dispatch_cmd;
mod gate_cmd;
mod invariant_cmds;
mod log_cmd;
mod merge_cmd;
mod plan_cmds;
mod pr_cmd;
mod report_cmd;
mod status_cmd;
mod tui;

use anyhow::Context;
use clap::{Parser, Subcommand};

use gator_core::token::guard;
use gator_db::pool;

use config::GatorConfig;

#[derive(Parser)]
#[command(name = "gator", about = "LLM coding agent fleet orchestrator")]
struct Cli {
    /// Database URL (overrides GATOR_DATABASE_URL env var)
    #[arg(long, global = true)]
    database_url: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Write a gator config file (no database required)
    Init {
        /// PostgreSQL connection URL
        #[arg(long, default_value = "postgresql://localhost:5432/gator")]
        db_url: String,
        /// Overwrite existing config file
        #[arg(long)]
        force: bool,
    },
    /// Initialize the gator database (requires config file or env vars)
    DbInit,
    /// Plan management
    Plan {
        #[command(subcommand)]
        command: PlanCommands,
    },
    /// Invariant management
    Invariant {
        #[command(subcommand)]
        command: InvariantCommands,
    },
    /// Dispatch a plan for execution
    Dispatch {
        /// Plan ID to dispatch
        plan_id: String,
        /// Maximum number of concurrent agents
        #[arg(long, default_value_t = 4)]
        max_agents: usize,
        /// Timeout per task in seconds
        #[arg(long, default_value_t = 1800)]
        timeout: u64,
    },
    /// Show plan status and task progress (omit plan_id to list all plans)
    Status {
        /// Plan ID to show status for (omit to list all plans)
        plan_id: Option<String>,
    },
    /// Show agent event log for a task
    Log {
        /// Task ID to show events for
        task_id: String,
        /// Filter to a specific attempt number
        #[arg(long)]
        attempt: Option<i32>,
    },
    /// Approve a task awaiting human review
    Approve {
        /// Task ID to approve
        task_id: String,
    },
    /// Reject a task awaiting human review (sends to failed for retry/escalation)
    Reject {
        /// Task ID to reject
        task_id: String,
    },
    /// Retry a failed or escalated task
    Retry {
        /// Task ID to retry
        task_id: String,
        /// Override retry_max limit
        #[arg(long)]
        force: bool,
    },
    /// View gate results for a task
    Gate {
        /// Task ID to view gate results for
        task_id: String,
    },
    /// Show token usage and duration report for a plan
    Report {
        /// Plan ID to report on
        plan_id: String,
    },
    /// Remove worktrees for completed tasks in a plan
    Cleanup {
        /// Plan ID to clean up
        plan_id: String,
        /// Remove worktrees for all tasks (not just passed)
        #[arg(long)]
        all: bool,
    },
    /// Merge passed task branches into the base branch
    Merge {
        /// Plan ID to merge
        plan_id: String,
        /// Show what would be merged without doing it
        #[arg(long)]
        dry_run: bool,
    },
    /// Create a GitHub PR from a completed plan
    Pr {
        /// Plan ID to create PR for
        plan_id: String,
        /// Create as a draft PR
        #[arg(long)]
        draft: bool,
        /// Override the base branch
        #[arg(long)]
        base: Option<String>,
    },
    /// Launch interactive TUI dashboard
    Dashboard,
    /// Read your assigned task (agent mode)
    Task,
    /// Run invariants for your task (agent mode)
    Check,
    /// Report progress (agent mode)
    Progress {
        /// Progress message to report
        message: String,
    },
    /// Signal task completion (agent mode)
    Done,
}

#[derive(Subcommand)]
pub enum PlanCommands {
    /// Create a plan from a TOML file
    Create {
        /// Path to the plan TOML file
        file: String,
    },
    /// Show plan details (or list all plans)
    Show {
        /// Plan ID to show (omit to list all)
        plan_id: Option<String>,
    },
    /// Approve a plan for execution
    Approve {
        /// Plan ID to approve
        plan_id: String,
    },
    /// Export a plan from the database as TOML
    Export {
        /// Plan ID to export
        plan_id: String,
        /// Output file path (defaults to stdout)
        #[arg(long)]
        output: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum InvariantCommands {
    /// Add a new invariant definition
    Add {
        /// Unique invariant name (e.g. rust_build)
        name: String,
        /// Kind of invariant: test_suite, typecheck, lint, coverage, custom
        #[arg(long)]
        kind: String,
        /// Command to execute (e.g. "cargo")
        #[arg(long)]
        command: String,
        /// Comma-separated arguments (e.g. "build,--workspace")
        #[arg(long)]
        args: Option<String>,
        /// Human-readable description
        #[arg(long)]
        description: Option<String>,
        /// Expected exit code (default: 0)
        #[arg(long, default_value_t = 0)]
        expected_exit_code: i32,
        /// Numeric threshold (e.g. coverage percentage)
        #[arg(long)]
        threshold: Option<f32>,
        /// Scope: global or project (default: project)
        #[arg(long, default_value = "project")]
        scope: String,
    },
    /// List all invariants
    List {
        /// Show full details for each invariant
        #[arg(long)]
        verbose: bool,
    },
    /// Test-run an invariant in the current directory
    Test {
        /// Invariant name to test
        name: String,
    },
}

/// Execute the `gator init` command: write config file.
fn cmd_init(db_url: &str, force: bool) -> anyhow::Result<()> {
    let path = config::config_path();

    if path.exists() && !force {
        anyhow::bail!(
            "config file already exists at {}\nUse --force to overwrite.",
            path.display()
        );
    }

    let token_secret = config::generate_token_secret();

    let cfg = config::ConfigFile {
        database: config::DatabaseSection {
            url: db_url.to_string(),
        },
        auth: config::AuthSection {
            token_secret: token_secret.clone(),
        },
    };

    config::save_config(&cfg)?;

    println!("Config written to {}", path.display());
    println!("  database.url = {db_url}");
    println!("  auth.token_secret = {}...{}", &token_secret[..8], &token_secret[56..]);
    println!();
    println!("Next: run `gator db-init` to create and migrate the database.");

    Ok(())
}

/// Execute the `gator db-init` command: create database and run migrations.
async fn cmd_db_init(cli_db_url: Option<&str>) -> anyhow::Result<()> {
    let resolved = GatorConfig::resolve(cli_db_url)?;

    println!("Initializing gator database...");

    // 1. Create the database if it does not exist.
    pool::ensure_database_exists(&resolved.db_config).await?;

    // 2. Connect to the target database.
    let db_pool = pool::create_pool(&resolved.db_config).await?;

    // 3. Run migrations.
    let migrations_path = pool::default_migrations_path();
    pool::run_migrations(&db_pool, migrations_path).await?;

    // 4. Print success with table counts.
    let counts = pool::table_counts(&db_pool).await?;
    println!("Database ready. Tables:");
    for (table, count) in &counts {
        println!("  {table}: {count} rows");
    }

    // 5. Clean shutdown.
    db_pool.close().await;

    println!("gator db-init complete.");
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    // -----------------------------------------------------------------
    // Agent-mode detection: if GATOR_AGENT_TOKEN is set, restrict the
    // command surface to the four agent-mode commands.
    // -----------------------------------------------------------------
    if guard::is_agent_mode() {
        // Resolve config for DB URL (best-effort).
        let db_config = GatorConfig::resolve(cli.database_url.as_deref())
            .map(|c| c.db_config)
            .unwrap_or_else(|_| gator_db::config::DbConfig::from_env());

        let pool_result = pool::create_pool(&db_config).await;
        let pool = pool_result.ok();

        let result = agent::run_agent_mode(cli.command, pool.as_ref()).await;

        // Clean shutdown if we have a pool.
        if let Some(p) = pool {
            p.close().await;
        }

        if let Err(e) = result {
            eprintln!("{e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // -----------------------------------------------------------------
    // Operator mode (default): full command surface.
    // -----------------------------------------------------------------
    match cli.command {
        Commands::Init { db_url, force } => {
            cmd_init(&db_url, force)?;
        }
        Commands::DbInit => {
            cmd_db_init(cli.database_url.as_deref()).await?;
        }
        Commands::Plan { command } => {
            let resolved = GatorConfig::resolve(cli.database_url.as_deref())?;
            let db_pool = pool::create_pool(&resolved.db_config).await?;
            let result = plan_cmds::run_plan_command(command, &db_pool).await;
            db_pool.close().await;
            result?;
        }
        Commands::Invariant { command } => {
            let resolved = GatorConfig::resolve(cli.database_url.as_deref())?;
            let db_pool = pool::create_pool(&resolved.db_config).await?;
            let result = invariant_cmds::run_invariant_command(command, &db_pool).await;
            db_pool.close().await;
            result?;
        }
        Commands::Dispatch {
            plan_id,
            max_agents,
            timeout,
        } => {
            let resolved = GatorConfig::resolve(cli.database_url.as_deref())?;
            let db_pool = pool::create_pool(&resolved.db_config).await?;
            let result = dispatch_cmd::run_dispatch(
                &db_pool,
                &plan_id,
                max_agents,
                timeout,
                &resolved.token_config,
            )
            .await;
            db_pool.close().await;
            result?;
        }
        Commands::Status { plan_id } => {
            let resolved = GatorConfig::resolve(cli.database_url.as_deref())?;
            let db_pool = pool::create_pool(&resolved.db_config).await?;
            let result = status_cmd::run_status(&db_pool, plan_id.as_deref()).await;
            db_pool.close().await;
            result?;
        }
        Commands::Log { task_id, attempt } => {
            let resolved = GatorConfig::resolve(cli.database_url.as_deref())?;
            let db_pool = pool::create_pool(&resolved.db_config).await?;
            let result = log_cmd::run_log(&db_pool, &task_id, attempt).await;
            db_pool.close().await;
            result?;
        }
        Commands::Approve { task_id } => {
            let resolved = GatorConfig::resolve(cli.database_url.as_deref())?;
            let db_pool = pool::create_pool(&resolved.db_config).await?;
            let id = uuid::Uuid::parse_str(&task_id)
                .with_context(|| format!("invalid task ID: {task_id}"))?;
            let result = gator_core::state::dispatch::approve_task(&db_pool, id).await;
            db_pool.close().await;
            result?;
            println!("Task {task_id} approved.");
        }
        Commands::Reject { task_id } => {
            let resolved = GatorConfig::resolve(cli.database_url.as_deref())?;
            let db_pool = pool::create_pool(&resolved.db_config).await?;
            let id = uuid::Uuid::parse_str(&task_id)
                .with_context(|| format!("invalid task ID: {task_id}"))?;
            let result = gator_core::state::dispatch::reject_task(&db_pool, id).await;
            db_pool.close().await;
            result?;
            println!("Task {task_id} rejected.");
        }
        Commands::Retry { task_id, force } => {
            let resolved = GatorConfig::resolve(cli.database_url.as_deref())?;
            let db_pool = pool::create_pool(&resolved.db_config).await?;
            let id = uuid::Uuid::parse_str(&task_id)
                .with_context(|| format!("invalid task ID: {task_id}"))?;
            let result =
                gator_core::state::dispatch::operator_retry_task(&db_pool, id, force).await;
            db_pool.close().await;
            result?;
            println!("Task {task_id} reset to pending for retry.");
        }
        Commands::Gate { task_id } => {
            let resolved = GatorConfig::resolve(cli.database_url.as_deref())?;
            let db_pool = pool::create_pool(&resolved.db_config).await?;
            let result = gate_cmd::run_gate(&db_pool, &task_id).await;
            db_pool.close().await;
            result?;
        }
        Commands::Report { plan_id } => {
            let resolved = GatorConfig::resolve(cli.database_url.as_deref())?;
            let db_pool = pool::create_pool(&resolved.db_config).await?;
            let result = report_cmd::run_report(&db_pool, &plan_id).await;
            db_pool.close().await;
            result?;
        }
        Commands::Cleanup { plan_id, all } => {
            let resolved = GatorConfig::resolve(cli.database_url.as_deref())?;
            let db_pool = pool::create_pool(&resolved.db_config).await?;
            let result = cleanup_cmd::run_cleanup(&db_pool, &plan_id, all).await;
            db_pool.close().await;
            result?;
        }
        Commands::Merge { plan_id, dry_run } => {
            let resolved = GatorConfig::resolve(cli.database_url.as_deref())?;
            let db_pool = pool::create_pool(&resolved.db_config).await?;
            let result = merge_cmd::run_merge(&db_pool, &plan_id, dry_run).await;
            db_pool.close().await;
            result?;
        }
        Commands::Pr {
            plan_id,
            draft,
            base,
        } => {
            let resolved = GatorConfig::resolve(cli.database_url.as_deref())?;
            let db_pool = pool::create_pool(&resolved.db_config).await?;
            let options = pr_cmd::PrOptions { draft, base };
            let result = pr_cmd::run_pr(&db_pool, &plan_id, &options).await;
            db_pool.close().await;
            result?;
        }
        Commands::Dashboard => {
            let resolved = GatorConfig::resolve(cli.database_url.as_deref())?;
            let db_pool = pool::create_pool(&resolved.db_config).await?;
            let result = tui::run_dashboard(db_pool.clone()).await;
            db_pool.close().await;
            result?;
        }
        Commands::Task => {
            println!("gator task: not available in operator mode (set GATOR_AGENT_TOKEN)");
        }
        Commands::Check => {
            println!("gator check: not available in operator mode (set GATOR_AGENT_TOKEN)");
        }
        Commands::Progress { message } => {
            println!("gator progress: not available in operator mode (set GATOR_AGENT_TOKEN)");
            let _ = message;
        }
        Commands::Done => {
            println!("gator done: not available in operator mode (set GATOR_AGENT_TOKEN)");
        }
    }

    Ok(())
}
