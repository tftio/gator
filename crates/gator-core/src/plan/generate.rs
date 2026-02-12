//! Plan generation: prompt construction and post-generation validation.
//!
//! Assembles project context into a system prompt for Claude Code, which
//! explores the codebase and produces a validated plan TOML. This module
//! contains pure logic (no I/O or subprocess spawning).

use std::path::Path;

use crate::plan::parser::PlanParseError;
use crate::plan::toml_format::{PlanMeta, PlanToml, TaskToml};
use crate::presets::{self, InvariantPreset};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Context assembled for the plan generation prompt.
#[derive(Debug, Clone)]
pub struct GenerateContext {
    /// Git base branch for the plan.
    pub base_branch: String,
    /// Detected project type (e.g. "rust", "node"), if any.
    pub project_type: Option<String>,
    /// Available invariants to reference in the plan.
    pub invariants: Vec<InvariantInfo>,
    /// Output file path where Claude should write the plan.
    pub output_path: String,
}

/// Simplified invariant description for prompt inclusion.
#[derive(Debug, Clone)]
pub struct InvariantInfo {
    /// Unique invariant name (e.g. `rust_build`).
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Kind: test_suite, typecheck, lint, coverage, custom.
    pub kind: String,
    /// Command to execute.
    pub command: String,
    /// Arguments to pass to the command.
    pub args: Vec<String>,
}

impl From<InvariantPreset> for InvariantInfo {
    fn from(p: InvariantPreset) -> Self {
        Self {
            name: p.name,
            description: p.description,
            kind: p.kind,
            command: p.command,
            args: p.args,
        }
    }
}

/// Errors from validating a generated plan file.
#[derive(Debug)]
pub enum GenerateValidationError {
    /// The output file was not created.
    FileNotFound {
        path: String,
        source: std::io::Error,
    },
    /// The output file exists but is empty.
    EmptyFile { path: String },
    /// The output file contains invalid plan TOML.
    Invalid {
        path: String,
        source: PlanParseError,
    },
}

impl std::fmt::Display for GenerateValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FileNotFound { path, source } => {
                write!(f, "plan file not found at {path:?}: {source}")
            }
            Self::EmptyFile { path } => write!(f, "plan file at {path:?} is empty"),
            Self::Invalid { path, source } => {
                write!(f, "plan file at {path:?} is invalid: {source}")
            }
        }
    }
}

impl std::error::Error for GenerateValidationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::FileNotFound { source, .. } => Some(source),
            Self::EmptyFile { .. } => None,
            Self::Invalid { source, .. } => Some(source),
        }
    }
}

// ---------------------------------------------------------------------------
// Context detection
// ---------------------------------------------------------------------------

/// Detect the base branch and project type for the given directory.
///
/// Returns `(base_branch, project_type)`. The base branch falls back to
/// `"main"` if detection fails. Project type is `None` if unrecognized.
pub fn detect_context(cwd: &Path, base_branch_override: Option<&str>) -> (String, Option<String>) {
    let base_branch = match base_branch_override {
        Some(b) => b.to_string(),
        None => presets::detect_base_branch(cwd),
    };
    let project_type = presets::detect_project_type(cwd);
    (base_branch, project_type)
}

/// Load invariant presets matching a project type.
///
/// If `project_type` is `None` or matches no presets, returns all presets
/// so the prompt includes the full invariant library for Claude to choose from.
pub fn invariants_from_presets(project_type: Option<&str>) -> Vec<InvariantInfo> {
    let presets = match project_type {
        Some(pt) => {
            let matched = presets::presets_for_project_type(pt);
            if matched.is_empty() {
                presets::load_presets()
            } else {
                matched
            }
        }
        None => presets::load_presets(),
    };
    presets.into_iter().map(InvariantInfo::from).collect()
}

// ---------------------------------------------------------------------------
// System prompt construction
// ---------------------------------------------------------------------------

/// TOML schema reference included in the system prompt.
const SCHEMA_REFERENCE: &str = r#"## Plan TOML Schema

```toml
[plan]
name = "string"           # REQUIRED. Human-readable plan name.
base_branch = "string"    # REQUIRED. Git branch to base task branches on.
# token_budget = 500000   # Optional. Total token budget (input + output).
# isolation = "worktree"  # Optional. "worktree" (default) or "container".
# container_image = "img" # Optional. Docker image for container isolation.

[[tasks]]
name = "string"           # REQUIRED. Unique task identifier (kebab-case).
description = """          # REQUIRED. Multi-line description for the agent.
Detailed instructions...
"""
scope = "narrow"           # REQUIRED. "narrow", "medium", or "broad".
gate = "auto"              # REQUIRED. "auto", "human_review", or "human_approve".
# retry_max = 3            # Optional. Max retries before escalation (default: 3).
# depends_on = ["other"]   # Optional. Task names this depends on.
invariants = ["name"]      # REQUIRED (should not be empty). Invariant names to check.
# harness = "claude-code"  # Optional. Override the default harness.
```

### Scope levels
- **narrow**: Single file or small change. Agent should finish quickly. Use `gate = "auto"`.
- **medium**: Multiple files, one module. Use `gate = "human_review"`.
- **broad**: Cross-cutting changes. Use `gate = "human_approve"`.

### Gate policies
- **auto**: Passes if all invariants pass. No human intervention.
- **human_review**: Invariants run, then a human reviews results before accepting.
- **human_approve**: Human must explicitly approve the task output.
"#;

/// Task decomposition guidelines included in the system prompt.
const DECOMPOSITION_GUIDELINES: &str = r#"## Decomposition Guidelines

1. **Prefer narrow tasks.** Each task should change as few files as possible. A task that touches 1-3 files is ideal.
2. **Define types first.** If multiple tasks share types or interfaces, create a task that defines them first and make others depend on it.
3. **Maximize parallelism.** Tasks without dependencies run concurrently. Structure the DAG to allow as much parallel work as possible.
4. **Diamond DAGs are good.** A common pattern: one setup task, N parallel implementation tasks, one integration task that depends on all of them.
5. **Write thorough descriptions.** The agent sees ONLY the task description (plus the codebase). Include:
   - Specific files to create or modify
   - Function signatures or type definitions when relevant
   - Edge cases to handle
   - What NOT to change
6. **Every task needs invariants.** Tasks without invariants cannot be auto-gated. Always include at least the build and test invariants.
7. **Use `depends_on` for data dependencies.** If task B reads a file that task A creates, B must depend on A.
8. **Keep task names kebab-case.** They become git branch suffixes.
"#;

/// Build the full system prompt for Claude Code.
///
/// The prompt includes the TOML schema, decomposition guidelines, and
/// project-specific context (base branch, project type, available invariants).
pub fn build_system_prompt(ctx: &GenerateContext) -> String {
    let mut prompt = String::with_capacity(4096);

    // Role and output contract.
    prompt.push_str("# Plan Architect for Gator\n\n");
    prompt.push_str(
        "You are a plan architect for Gator, an LLM agent orchestrator. \
         Your job is to decompose a feature request into a dependency-ordered \
         set of tasks that coding agents will execute independently.\n\n",
    );
    prompt.push_str(&format!(
        "Write the plan TOML to `{}` using your Write tool. \
         Do NOT print the TOML to stdout -- write it to the file.\n\n",
        ctx.output_path
    ));
    prompt.push_str(
        "IMPORTANT: You are writing a PLAN, not implementing the feature. \
         Do NOT create or modify source code files. Do NOT write implementation code. \
         Your sole deliverable is one TOML file describing tasks for other agents to execute.\n\n",
    );
    prompt.push_str(
        "Before writing the plan, explore the codebase using Read, Glob, Grep, and Bash \
         to understand the project structure, existing patterns, and where changes need to go. \
         The quality of the plan depends on understanding the codebase.\n\n",
    );

    // Schema reference.
    prompt.push_str(SCHEMA_REFERENCE);
    prompt.push('\n');

    // Decomposition guidelines.
    prompt.push_str(DECOMPOSITION_GUIDELINES);
    prompt.push('\n');

    // Project context.
    prompt.push_str("## Project Context\n\n");
    prompt.push_str(&format!("- **Base branch:** `{}`\n", ctx.base_branch));

    match &ctx.project_type {
        Some(pt) => prompt.push_str(&format!("- **Project type:** {pt}\n")),
        None => prompt.push_str("- **Project type:** unknown (could not auto-detect)\n"),
    }

    // Available invariants.
    if ctx.invariants.is_empty() {
        prompt.push_str("- **Available invariants:** none detected\n");
    } else {
        prompt.push_str("\n### Available Invariants\n\n");
        prompt.push_str(
            "Use these names in each task's `invariants` array. \
             Every task should include at least the build and test invariants.\n\n",
        );
        for inv in &ctx.invariants {
            prompt.push_str(&format!(
                "- `{}` ({}) -- {} (`{} {}`)\n",
                inv.name,
                inv.kind,
                inv.description,
                inv.command,
                inv.args.join(" ")
            ));
        }
    }

    prompt
}

// ---------------------------------------------------------------------------
// Meta-plan construction
// ---------------------------------------------------------------------------

/// Build a `PlanToml` for a meta-plan that generates a plan TOML file.
///
/// The meta-plan has a single task ("write-plan") that runs Claude Code with
/// the given system prompt. It references two invariants that validate the
/// generated plan file exists and parses correctly.
pub fn build_meta_plan(system_prompt: &str, base_branch: &str, gate: &str) -> PlanToml {
    let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    PlanToml {
        plan: PlanMeta {
            id: None,
            name: format!("_plan-gen-{timestamp}"),
            base_branch: base_branch.to_string(),
            token_budget: None,
            default_harness: "claude-code".to_string(),
            isolation: "worktree".to_string(),
            container_image: None,
        },
        tasks: vec![TaskToml {
            name: "write-plan".to_string(),
            description: system_prompt.to_string(),
            scope: "narrow".to_string(),
            gate: gate.to_string(),
            retry_max: 2,
            depends_on: vec![],
            invariants: vec![
                "_gator_plan_file_exists".to_string(),
                "_gator_plan_validates".to_string(),
            ],
            harness: None,
        }],
    }
}

// ---------------------------------------------------------------------------
// Post-generation validation
// ---------------------------------------------------------------------------

/// Read and validate a generated plan TOML file.
///
/// Returns the parsed `PlanToml` on success, or a descriptive error.
pub fn validate_generated_plan(path: &str) -> Result<PlanToml, GenerateValidationError> {
    let content =
        std::fs::read_to_string(path).map_err(|e| GenerateValidationError::FileNotFound {
            path: path.to_string(),
            source: e,
        })?;

    if content.trim().is_empty() {
        return Err(GenerateValidationError::EmptyFile {
            path: path.to_string(),
        });
    }

    crate::plan::parse_plan_toml(&content).map_err(|e| GenerateValidationError::Invalid {
        path: path.to_string(),
        source: e,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_invariants() -> Vec<InvariantInfo> {
        vec![
            InvariantInfo {
                name: "rust_build".to_string(),
                description: "Build the workspace".to_string(),
                kind: "typecheck".to_string(),
                command: "cargo".to_string(),
                args: vec!["build".to_string(), "--workspace".to_string()],
            },
            InvariantInfo {
                name: "rust_test".to_string(),
                description: "Run all tests".to_string(),
                kind: "test_suite".to_string(),
                command: "cargo".to_string(),
                args: vec!["test".to_string(), "--workspace".to_string()],
            },
        ]
    }

    fn sample_context() -> GenerateContext {
        GenerateContext {
            base_branch: "main".to_string(),
            project_type: Some("rust".to_string()),
            invariants: sample_invariants(),
            output_path: "plan.toml".to_string(),
        }
    }

    // -- build_system_prompt tests --

    #[test]
    fn prompt_contains_schema_markers() {
        let prompt = build_system_prompt(&sample_context());
        assert!(prompt.contains("Plan TOML Schema"));
        assert!(prompt.contains("[plan]"));
        assert!(prompt.contains("[[tasks]]"));
        assert!(prompt.contains("scope ="));
        assert!(prompt.contains("gate ="));
        assert!(prompt.contains("invariants ="));
    }

    #[test]
    fn prompt_contains_decomposition_guidelines() {
        let prompt = build_system_prompt(&sample_context());
        assert!(prompt.contains("Decomposition Guidelines"));
        assert!(prompt.contains("Prefer narrow tasks"));
        assert!(prompt.contains("Diamond DAGs"));
    }

    #[test]
    fn prompt_includes_invariants() {
        let prompt = build_system_prompt(&sample_context());
        assert!(prompt.contains("rust_build"));
        assert!(prompt.contains("rust_test"));
        assert!(prompt.contains("cargo build --workspace"));
        assert!(prompt.contains("cargo test --workspace"));
    }

    #[test]
    fn prompt_includes_project_context() {
        let prompt = build_system_prompt(&sample_context());
        assert!(prompt.contains("Base branch:** `main`"));
        assert!(prompt.contains("Project type:** rust"));
    }

    #[test]
    fn prompt_includes_output_path() {
        let ctx = GenerateContext {
            output_path: "auth-plan.toml".to_string(),
            ..sample_context()
        };
        let prompt = build_system_prompt(&ctx);
        assert!(prompt.contains("auth-plan.toml"));
    }

    #[test]
    fn prompt_handles_empty_invariants() {
        let ctx = GenerateContext {
            invariants: vec![],
            ..sample_context()
        };
        let prompt = build_system_prompt(&ctx);
        assert!(prompt.contains("none detected"));
        assert!(!prompt.contains("Available Invariants"));
    }

    #[test]
    fn prompt_handles_unknown_project_type() {
        let ctx = GenerateContext {
            project_type: None,
            ..sample_context()
        };
        let prompt = build_system_prompt(&ctx);
        assert!(prompt.contains("unknown (could not auto-detect)"));
    }

    #[test]
    fn prompt_instructs_file_write() {
        let prompt = build_system_prompt(&sample_context());
        assert!(prompt.contains("Write tool"));
        assert!(prompt.contains("Do NOT print the TOML to stdout"));
        assert!(prompt.contains("Do NOT create or modify source code files"));
    }

    // -- detect_context tests --

    #[test]
    fn detect_context_with_override() {
        let dir = TempDir::new().unwrap();
        let (branch, _) = detect_context(dir.path(), Some("develop"));
        assert_eq!(branch, "develop");
    }

    #[test]
    fn detect_context_without_override_fallback() {
        let dir = TempDir::new().unwrap();
        // No git repo => falls back to "main".
        let (branch, project_type) = detect_context(dir.path(), None);
        assert_eq!(branch, "main");
        assert!(project_type.is_none());
    }

    #[test]
    fn detect_context_rust_project() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        let (_, project_type) = detect_context(dir.path(), None);
        assert_eq!(project_type, Some("rust".to_string()));
    }

    // -- invariants_from_presets tests --

    #[test]
    fn invariants_for_rust() {
        let invs = invariants_from_presets(Some("rust"));
        let names: Vec<&str> = invs.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"rust_build"));
        assert!(names.contains(&"rust_test"));
        assert!(names.contains(&"rust_clippy"));
    }

    #[test]
    fn invariants_for_unknown_returns_all() {
        let invs = invariants_from_presets(Some("brainfuck"));
        // Should return all presets since "brainfuck" matches nothing.
        assert!(invs.len() >= 4, "expected all presets, got {}", invs.len());
    }

    #[test]
    fn invariants_for_none_returns_all() {
        let invs = invariants_from_presets(None);
        assert!(invs.len() >= 4, "expected all presets, got {}", invs.len());
    }

    // -- validate_generated_plan tests --

    #[test]
    fn validate_catches_missing_file() {
        let result = validate_generated_plan("/nonexistent/path/plan.toml");
        assert!(matches!(
            result,
            Err(GenerateValidationError::FileNotFound { .. })
        ));
    }

    #[test]
    fn validate_catches_empty_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("empty.toml");
        std::fs::write(&path, "  \n").unwrap();
        let result = validate_generated_plan(path.to_str().unwrap());
        assert!(matches!(
            result,
            Err(GenerateValidationError::EmptyFile { .. })
        ));
    }

    #[test]
    fn validate_catches_invalid_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "this is not valid toml {{{").unwrap();
        let result = validate_generated_plan(path.to_str().unwrap());
        assert!(matches!(
            result,
            Err(GenerateValidationError::Invalid { .. })
        ));
    }

    #[test]
    fn validate_accepts_valid_plan() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("good.toml");
        std::fs::write(
            &path,
            r#"
[plan]
name = "Test"
base_branch = "main"

[[tasks]]
name = "t1"
description = "Do something"
scope = "narrow"
gate = "auto"
invariants = ["check"]
"#,
        )
        .unwrap();
        let result = validate_generated_plan(path.to_str().unwrap());
        assert!(result.is_ok());
        let plan = result.unwrap();
        assert_eq!(plan.plan.name, "Test");
        assert_eq!(plan.tasks.len(), 1);
    }

    // -- InvariantInfo From<InvariantPreset> --

    #[test]
    fn invariant_info_from_preset() {
        let preset = InvariantPreset {
            name: "rust_build".to_string(),
            project_type: "rust".to_string(),
            description: "Build".to_string(),
            kind: "typecheck".to_string(),
            command: "cargo".to_string(),
            args: vec!["build".to_string()],
        };
        let info = InvariantInfo::from(preset);
        assert_eq!(info.name, "rust_build");
        assert_eq!(info.command, "cargo");
    }

    // -- build_meta_plan tests --

    #[test]
    fn build_meta_plan_structure() {
        let plan = build_meta_plan("Write a plan", "main", "human_review");

        assert!(plan.plan.name.starts_with("_plan-gen-"));
        assert_eq!(plan.plan.base_branch, "main");
        assert_eq!(plan.plan.default_harness, "claude-code");
        assert_eq!(plan.plan.isolation, "worktree");
        assert!(plan.plan.token_budget.is_none());
        assert!(plan.plan.container_image.is_none());

        assert_eq!(plan.tasks.len(), 1);
        let task = &plan.tasks[0];
        assert_eq!(task.name, "write-plan");
        assert_eq!(task.description, "Write a plan");
        assert_eq!(task.scope, "narrow");
        assert_eq!(task.gate, "human_review");
        assert_eq!(task.retry_max, 2);
        assert!(task.depends_on.is_empty());
        assert_eq!(
            task.invariants,
            vec!["_gator_plan_file_exists", "_gator_plan_validates"]
        );
        assert!(task.harness.is_none());
    }

    #[test]
    fn build_meta_plan_uses_timestamp() {
        let plan = build_meta_plan("prompt", "develop", "auto");
        assert!(
            plan.plan.name.starts_with("_plan-gen-"),
            "name should start with _plan-gen-, got: {}",
            plan.plan.name
        );
        // Name should be longer than just the prefix (has timestamp).
        assert!(plan.plan.name.len() > "_plan-gen-".len());
    }

    #[test]
    fn build_meta_plan_respects_gate_policy() {
        let auto = build_meta_plan("p", "main", "auto");
        assert_eq!(auto.tasks[0].gate, "auto");

        let approve = build_meta_plan("p", "main", "human_approve");
        assert_eq!(approve.tasks[0].gate, "human_approve");
    }
}
