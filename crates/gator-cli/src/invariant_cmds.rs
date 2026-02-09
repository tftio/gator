//! Operator-mode CLI handlers for `gator invariant` subcommands.
//!
//! Implements:
//! - `gator invariant add`           -- create a new invariant definition
//! - `gator invariant list`          -- list all invariants in table format
//! - `gator invariant test`          -- test-run an invariant in the current directory
//! - `gator invariant presets list`  -- list available preset invariants
//! - `gator invariant presets install` -- register preset invariants in the database

use anyhow::{Context, Result, bail};
use sqlx::PgPool;

use gator_core::invariant::runner::{self, InvariantResult};
use gator_core::presets;
use gator_db::models::{InvariantKind, InvariantScope};
use gator_db::queries::invariants;

use crate::{InvariantCommands, PresetCommands};

// -----------------------------------------------------------------------
// Public entry point
// -----------------------------------------------------------------------

/// Dispatch an `InvariantCommands` variant to the appropriate handler.
pub async fn run_invariant_command(command: InvariantCommands, pool: &PgPool) -> Result<()> {
    match command {
        InvariantCommands::Add {
            name,
            kind,
            command: cmd,
            args,
            description,
            expected_exit_code,
            threshold,
            scope,
            timeout,
        } => {
            cmd_add(
                pool,
                AddParams {
                    name,
                    kind,
                    command: cmd,
                    args,
                    description,
                    expected_exit_code,
                    threshold,
                    scope,
                    timeout,
                },
            )
            .await
        }
        InvariantCommands::List { verbose } => cmd_list(pool, verbose).await,
        InvariantCommands::Test { name } => cmd_test(pool, &name).await,
        InvariantCommands::Presets { command } => match command {
            PresetCommands::List { project_type } => {
                cmd_presets_list(project_type.as_deref())
            }
            PresetCommands::Install { project_type } => {
                cmd_presets_install(pool, project_type.as_deref()).await
            }
        },
    }
}

// -----------------------------------------------------------------------
// gator invariant add
// -----------------------------------------------------------------------

/// Grouped parameters for the `add` command to avoid too many function args.
struct AddParams {
    name: String,
    kind: String,
    command: String,
    args: Option<String>,
    description: Option<String>,
    expected_exit_code: i32,
    threshold: Option<f32>,
    scope: String,
    timeout: i32,
}

/// Create a new invariant definition and insert it into the database.
async fn cmd_add(pool: &PgPool, params: AddParams) -> Result<()> {
    // Parse the kind enum.
    let kind: InvariantKind = params.kind.parse().map_err(|_| {
        anyhow::anyhow!(
            "invalid invariant kind {:?}; expected one of: \
             test_suite, typecheck, lint, coverage, custom",
            params.kind,
        )
    })?;

    // Parse the scope enum.
    let scope: InvariantScope = params.scope.parse().map_err(|_| {
        anyhow::anyhow!(
            "invalid invariant scope {:?}; expected one of: global, project",
            params.scope,
        )
    })?;

    // Parse comma-separated args into a Vec<String>.
    let args_vec: Vec<String> = match params.args.as_deref() {
        Some(s) if !s.is_empty() => s.split(',').map(|a| a.to_owned()).collect(),
        _ => Vec::new(),
    };

    let new = invariants::NewInvariant {
        name: &params.name,
        description: params.description.as_deref(),
        kind,
        command: &params.command,
        args: &args_vec,
        expected_exit_code: params.expected_exit_code,
        threshold: params.threshold,
        scope,
        timeout_secs: params.timeout,
    };

    let invariant = invariants::insert_invariant(pool, &new)
        .await
        .with_context(|| {
            format!(
                "failed to add invariant {:?} (is the name already taken?)",
                params.name,
            )
        })?;

    println!("Invariant created:");
    println!("  ID:      {}", invariant.id);
    println!("  Name:    {}", invariant.name);
    println!("  Kind:    {}", invariant.kind);
    println!("  Command: {}", invariant.command);
    if !invariant.args.is_empty() {
        println!("  Args:    {}", invariant.args.join(" "));
    }
    println!("  Scope:   {}", invariant.scope);

    Ok(())
}

// -----------------------------------------------------------------------
// gator invariant list
// -----------------------------------------------------------------------

/// List all invariants in a table format, optionally with full details.
async fn cmd_list(pool: &PgPool, verbose: bool) -> Result<()> {
    let invs = invariants::list_invariants(pool).await?;

    if invs.is_empty() {
        println!("No invariants found. Use `gator invariant add` to create one.");
        return Ok(());
    }

    if verbose {
        for (i, inv) in invs.iter().enumerate() {
            if i > 0 {
                println!("---");
            }
            println!("Name:              {}", inv.name);
            println!("ID:                {}", inv.id);
            println!("Kind:              {}", inv.kind);
            println!("Command:           {}", inv.command);
            if !inv.args.is_empty() {
                println!("Args:              {}", inv.args.join(" "));
            }
            if let Some(desc) = &inv.description {
                println!("Description:       {}", desc);
            }
            println!("Expected exit:     {}", inv.expected_exit_code);
            if let Some(t) = inv.threshold {
                println!("Threshold:         {}", t);
            }
            println!("Scope:             {}", inv.scope);
            println!("Created:           {}", inv.created_at);
        }
    } else {
        // Table format: fixed-width columns.
        // Compute column widths.
        let name_w = invs.iter().map(|i| i.name.len()).max().unwrap_or(4).max(4);
        let kind_w = invs
            .iter()
            .map(|i| i.kind.to_string().len())
            .max()
            .unwrap_or(4)
            .max(4);
        let cmd_w = invs
            .iter()
            .map(|i| i.command.len())
            .max()
            .unwrap_or(7)
            .max(7);
        let scope_w = invs
            .iter()
            .map(|i| i.scope.to_string().len())
            .max()
            .unwrap_or(5)
            .max(5);

        // Header
        println!(
            "{:<name_w$}  {:<kind_w$}  {:<cmd_w$}  {:<scope_w$}",
            "NAME", "KIND", "COMMAND", "SCOPE",
        );

        // Rows
        for inv in &invs {
            println!(
                "{:<name_w$}  {:<kind_w$}  {:<cmd_w$}  {:<scope_w$}",
                inv.name, inv.kind, inv.command, inv.scope,
            );
        }
    }

    Ok(())
}

// -----------------------------------------------------------------------
// gator invariant test <name>
// -----------------------------------------------------------------------

/// Look up an invariant by name, run it in the current working directory,
/// and print the results.
///
/// Exits with code 0 if the invariant passed, or returns an error (which
/// causes the process to exit with code 1) if it failed.
async fn cmd_test(pool: &PgPool, name: &str) -> Result<()> {
    let invariant = invariants::get_invariant_by_name(pool, name)
        .await?
        .with_context(|| format!("invariant {:?} not found", name))?;

    let cwd = std::env::current_dir().context("failed to get current directory")?;

    println!("Running invariant {:?}...", invariant.name);
    println!(
        "  command: {} {}",
        invariant.command,
        invariant.args.join(" "),
    );
    println!("  working directory: {}", cwd.display());
    println!();

    let result: InvariantResult = runner::run_invariant(&invariant, &cwd).await?;

    // Status line
    let status_label = if result.passed { "PASSED" } else { "FAILED" };
    println!("Result: {}", status_label);
    println!(
        "  Exit code: {}",
        result
            .exit_code
            .map_or("unknown (signal)".to_owned(), |c| c.to_string()),
    );
    println!("  Duration:  {}ms", result.duration_ms);

    // Stdout (truncated)
    if !result.stdout.is_empty() {
        println!();
        println!("--- stdout (truncated) ---");
        print_truncated(&result.stdout, 2000);
    }

    // Stderr (truncated)
    if !result.stderr.is_empty() {
        println!();
        println!("--- stderr (truncated) ---");
        print_truncated(&result.stderr, 2000);
    }

    if result.passed {
        Ok(())
    } else {
        bail!(
            "invariant {:?} failed (exit code {:?})",
            name,
            result.exit_code,
        );
    }
}

/// Print `text`, truncating to at most `max_chars` characters.
fn print_truncated(text: &str, max_chars: usize) {
    let trimmed = text.trim();
    if trimmed.len() <= max_chars {
        println!("{}", trimmed);
    } else {
        println!("{}...", &trimmed[..max_chars]);
    }
}

// -----------------------------------------------------------------------
// gator invariant presets list [--project-type <type>]
// -----------------------------------------------------------------------

/// List available preset invariants, optionally filtered by project type.
fn cmd_presets_list(project_type_filter: Option<&str>) -> Result<()> {
    let all_presets = presets::load_presets();

    let filtered: Vec<&presets::InvariantPreset> = match project_type_filter {
        Some(pt) => {
            let known = presets::available_project_types();
            if !known.contains(&pt.to_string()) {
                bail!(
                    "unknown project type {:?}; available types: {}",
                    pt,
                    known.join(", ")
                );
            }
            all_presets.iter().filter(|p| p.project_type == pt).collect()
        }
        None => all_presets.iter().collect(),
    };

    if filtered.is_empty() {
        println!("No presets found.");
        return Ok(());
    }

    // Compute column widths.
    let name_w = filtered
        .iter()
        .map(|p| p.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let type_w = filtered
        .iter()
        .map(|p| p.project_type.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let kind_w = filtered
        .iter()
        .map(|p| p.kind.len())
        .max()
        .unwrap_or(4)
        .max(4);

    // Header
    println!(
        "{:<name_w$}  {:<type_w$}  {:<kind_w$}  DESCRIPTION",
        "NAME", "TYPE", "KIND",
    );

    // Rows
    for preset in &filtered {
        println!(
            "{:<name_w$}  {:<type_w$}  {:<kind_w$}  {}",
            preset.name, preset.project_type, preset.kind, preset.description,
        );
    }

    println!();
    println!(
        "{} preset(s) available.",
        filtered.len()
    );

    Ok(())
}

// -----------------------------------------------------------------------
// gator invariant presets install [--project-type <type>]
// -----------------------------------------------------------------------

/// Detect project type (or use override) and register matching preset
/// invariants in the database. Skips any that already exist.
async fn cmd_presets_install(pool: &PgPool, project_type_override: Option<&str>) -> Result<()> {
    let project_type = match project_type_override {
        Some(pt) => {
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
        None => {
            let cwd = std::env::current_dir().context("failed to get current directory")?;
            match presets::detect_project_type(&cwd) {
                Some(pt) => {
                    println!("Detected project type: {}", pt);
                    pt
                }
                None => {
                    bail!(
                        "could not detect project type. Use --project-type to specify one of: {}",
                        presets::available_project_types().join(", ")
                    );
                }
            }
        }
    };

    let matching = presets::presets_for_project_type(&project_type);
    if matching.is_empty() {
        println!("No presets defined for project type {:?}.", project_type);
        return Ok(());
    }

    let mut registered = vec![];
    let mut skipped = vec![];

    for preset in &matching {
        let existing = invariants::get_invariant_by_name(pool, &preset.name).await?;
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

        let new = invariants::NewInvariant {
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

        invariants::insert_invariant(pool, &new).await?;
        registered.push(preset.name.clone());
    }

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

    Ok(())
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    // A minimal top-level parser for testing subcommand parsing.
    #[derive(Parser)]
    #[command(name = "gator")]
    struct TestCli {
        #[command(subcommand)]
        command: TestCommands,
    }

    #[derive(clap::Subcommand)]
    enum TestCommands {
        Invariant {
            #[command(subcommand)]
            command: InvariantCommands,
        },
    }

    // -- Preset subcommand parsing tests --

    #[test]
    fn clap_parses_presets_list() {
        let cli = TestCli::try_parse_from(["gator", "invariant", "presets", "list"])
            .expect("should parse");
        match cli.command {
            TestCommands::Invariant {
                command: InvariantCommands::Presets {
                    command: PresetCommands::List { project_type },
                },
            } => {
                assert!(project_type.is_none());
            }
            _ => panic!("expected Invariant Presets List"),
        }
    }

    #[test]
    fn clap_parses_presets_list_with_type() {
        let cli = TestCli::try_parse_from([
            "gator",
            "invariant",
            "presets",
            "list",
            "--project-type",
            "rust",
        ])
        .expect("should parse");
        match cli.command {
            TestCommands::Invariant {
                command: InvariantCommands::Presets {
                    command: PresetCommands::List { project_type },
                },
            } => {
                assert_eq!(project_type.as_deref(), Some("rust"));
            }
            _ => panic!("expected Invariant Presets List"),
        }
    }

    #[test]
    fn clap_parses_presets_install() {
        let cli = TestCli::try_parse_from(["gator", "invariant", "presets", "install"])
            .expect("should parse");
        match cli.command {
            TestCommands::Invariant {
                command: InvariantCommands::Presets {
                    command: PresetCommands::Install { project_type },
                },
            } => {
                assert!(project_type.is_none());
            }
            _ => panic!("expected Invariant Presets Install"),
        }
    }

    #[test]
    fn clap_parses_presets_install_with_type() {
        let cli = TestCli::try_parse_from([
            "gator",
            "invariant",
            "presets",
            "install",
            "--project-type",
            "python",
        ])
        .expect("should parse");
        match cli.command {
            TestCommands::Invariant {
                command: InvariantCommands::Presets {
                    command: PresetCommands::Install { project_type },
                },
            } => {
                assert_eq!(project_type.as_deref(), Some("python"));
            }
            _ => panic!("expected Invariant Presets Install"),
        }
    }

    // -- Presets list output tests (no DB needed) --

    #[test]
    fn presets_list_all_succeeds() {
        cmd_presets_list(None).expect("listing all presets should succeed");
    }

    #[test]
    fn presets_list_rust_succeeds() {
        cmd_presets_list(Some("rust")).expect("listing rust presets should succeed");
    }

    #[test]
    fn presets_list_unknown_type_fails() {
        let result = cmd_presets_list(Some("cobol"));
        assert!(result.is_err());
    }

    // -- Enum parsing tests --

    #[test]
    fn parse_kind_valid() {
        assert_eq!(
            "typecheck".parse::<InvariantKind>().unwrap(),
            InvariantKind::Typecheck,
        );
        assert_eq!(
            "test_suite".parse::<InvariantKind>().unwrap(),
            InvariantKind::TestSuite,
        );
        assert_eq!(
            "lint".parse::<InvariantKind>().unwrap(),
            InvariantKind::Lint,
        );
        assert_eq!(
            "coverage".parse::<InvariantKind>().unwrap(),
            InvariantKind::Coverage,
        );
        assert_eq!(
            "custom".parse::<InvariantKind>().unwrap(),
            InvariantKind::Custom,
        );
    }

    #[test]
    fn parse_kind_invalid() {
        assert!("bogus".parse::<InvariantKind>().is_err());
    }

    #[test]
    fn parse_scope_valid() {
        assert_eq!(
            "global".parse::<InvariantScope>().unwrap(),
            InvariantScope::Global,
        );
        assert_eq!(
            "project".parse::<InvariantScope>().unwrap(),
            InvariantScope::Project,
        );
    }

    #[test]
    fn parse_scope_invalid() {
        assert!("local".parse::<InvariantScope>().is_err());
    }

    // -- CSV arg parsing tests --

    #[test]
    fn csv_args_parsing() {
        let csv = "build,--workspace,--release";
        let args: Vec<String> = csv.split(',').map(|a| a.to_owned()).collect();
        assert_eq!(args, vec!["build", "--workspace", "--release"]);
    }

    #[test]
    fn csv_args_empty() {
        let csv = "";
        let args: Vec<String> = if csv.is_empty() {
            Vec::new()
        } else {
            csv.split(',').map(|a| a.to_owned()).collect()
        };
        assert!(args.is_empty());
    }

    // -- Truncation tests --

    #[test]
    fn print_truncated_within_limit() {
        // Verify it does not panic.
        let text = "hello world";
        print_truncated(text, 100);
    }

    #[test]
    fn print_truncated_over_limit() {
        // Verify it does not panic when truncating.
        let text = "abcdefghij";
        print_truncated(text, 5);
    }

    // -- Clap CLI parsing tests --

    #[test]
    fn clap_parses_add_with_all_options() {
        let cli = TestCli::try_parse_from([
            "gator",
            "invariant",
            "add",
            "rust_build",
            "--kind",
            "typecheck",
            "--command",
            "cargo",
            "--args",
            "build,--workspace",
            "--description",
            "Verify Rust workspace builds",
            "--expected-exit-code",
            "0",
            "--threshold",
            "80.0",
            "--scope",
            "global",
            "--timeout",
            "60",
        ])
        .expect("should parse successfully");

        match cli.command {
            TestCommands::Invariant {
                command:
                    InvariantCommands::Add {
                        name,
                        kind,
                        command,
                        args,
                        description,
                        expected_exit_code,
                        threshold,
                        scope,
                        timeout,
                    },
            } => {
                assert_eq!(name, "rust_build");
                assert_eq!(kind, "typecheck");
                assert_eq!(command, "cargo");
                assert_eq!(args.as_deref(), Some("build,--workspace"));
                assert_eq!(description.as_deref(), Some("Verify Rust workspace builds"));
                assert_eq!(expected_exit_code, 0);
                assert_eq!(threshold, Some(80.0));
                assert_eq!(scope, "global");
                assert_eq!(timeout, 60);
            }
            _ => panic!("expected Invariant Add command"),
        }
    }

    #[test]
    fn clap_parses_add_with_required_only() {
        let cli = TestCli::try_parse_from([
            "gator",
            "invariant",
            "add",
            "my_check",
            "--kind",
            "custom",
            "--command",
            "echo",
        ])
        .expect("should parse successfully");

        match cli.command {
            TestCommands::Invariant {
                command:
                    InvariantCommands::Add {
                        name,
                        kind,
                        command,
                        args,
                        description,
                        expected_exit_code,
                        threshold,
                        scope,
                        timeout,
                    },
            } => {
                assert_eq!(name, "my_check");
                assert_eq!(kind, "custom");
                assert_eq!(command, "echo");
                assert!(args.is_none());
                assert!(description.is_none());
                assert_eq!(expected_exit_code, 0); // default
                assert!(threshold.is_none());
                assert_eq!(scope, "project"); // default
                assert_eq!(timeout, 300); // default
            }
            _ => panic!("expected Invariant Add command"),
        }
    }

    #[test]
    fn clap_add_missing_kind_fails() {
        let result =
            TestCli::try_parse_from(["gator", "invariant", "add", "my_check", "--command", "echo"]);
        assert!(result.is_err(), "missing --kind should fail");
    }

    #[test]
    fn clap_add_missing_command_fails() {
        let result =
            TestCli::try_parse_from(["gator", "invariant", "add", "my_check", "--kind", "custom"]);
        assert!(result.is_err(), "missing --command should fail");
    }

    #[test]
    fn clap_add_missing_name_fails() {
        let result = TestCli::try_parse_from([
            "gator",
            "invariant",
            "add",
            "--kind",
            "custom",
            "--command",
            "echo",
        ]);
        assert!(result.is_err(), "missing name should fail");
    }

    #[test]
    fn clap_parses_list_default() {
        let cli = TestCli::try_parse_from(["gator", "invariant", "list"]).expect("should parse");
        match cli.command {
            TestCommands::Invariant {
                command: InvariantCommands::List { verbose },
            } => {
                assert!(!verbose);
            }
            _ => panic!("expected Invariant List"),
        }
    }

    #[test]
    fn clap_parses_list_verbose() {
        let cli = TestCli::try_parse_from(["gator", "invariant", "list", "--verbose"])
            .expect("should parse");
        match cli.command {
            TestCommands::Invariant {
                command: InvariantCommands::List { verbose },
            } => {
                assert!(verbose);
            }
            _ => panic!("expected Invariant List"),
        }
    }

    #[test]
    fn clap_parses_test_subcommand() {
        let cli = TestCli::try_parse_from(["gator", "invariant", "test", "my_check"])
            .expect("should parse");
        match cli.command {
            TestCommands::Invariant {
                command: InvariantCommands::Test { name },
            } => {
                assert_eq!(name, "my_check");
            }
            _ => panic!("expected Invariant Test"),
        }
    }

    #[test]
    fn clap_test_missing_name_fails() {
        let result = TestCli::try_parse_from(["gator", "invariant", "test"]);
        assert!(result.is_err(), "missing name should fail");
    }
}
