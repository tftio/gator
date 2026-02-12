//! TOML format types for plan definition files.
//!
//! These types map directly to the `plan.toml` on-disk format and are
//! deserialized via `serde` + the `toml` crate.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Top-level structure of a `plan.toml` file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanToml {
    /// Plan metadata.
    pub plan: PlanMeta,
    /// Tasks within the plan.
    #[serde(default)]
    pub tasks: Vec<TaskToml>,
}

/// Plan-level metadata in `[plan]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanMeta {
    /// Plan UUID, set after `gator plan create` writes the plan to the database.
    /// Absent in authored plan files, present once the plan has been created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    /// Human-readable plan name.
    pub name: String,
    /// Git branch to use as the base for task branches.
    pub base_branch: String,
    /// Optional total token budget (input + output). NULL means unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<i64>,
    /// Default harness to use for tasks that don't specify one.
    #[serde(default = "default_harness_name")]
    pub default_harness: String,
    /// Isolation mode: "worktree" or "container".
    #[serde(default = "default_isolation")]
    pub isolation: String,
    /// Docker image to use for container isolation (e.g. "gator-agent:latest").
    /// Only used when `isolation = "container"`. Falls back to "ubuntu:24.04".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_image: Option<String>,
}

/// A single `[[tasks]]` entry in the plan TOML.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskToml {
    /// Unique task name within the plan (used as an identifier in `depends_on`).
    pub name: String,
    /// Multi-line description of what the task should accomplish.
    pub description: String,
    /// Scope level: "narrow", "medium", or "broad".
    pub scope: String,
    /// Gate policy: "auto", "human_review", or "human_approve".
    pub gate: String,
    /// Maximum retry attempts before escalation.
    #[serde(default = "default_retry_max")]
    pub retry_max: i32,
    /// Names of tasks this task depends on (must complete first).
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Names of invariants to link to this task.
    #[serde(default)]
    pub invariants: Vec<String>,
    /// Override harness for this task (uses plan default_harness if not set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
}

fn default_retry_max() -> i32 {
    3
}

fn default_harness_name() -> String {
    "claude-code".to_string()
}

fn default_isolation() -> String {
    "worktree".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimal_plan() {
        let toml_str = r#"
[plan]
name = "Test plan"
base_branch = "main"

[[tasks]]
name = "task-one"
description = "Do something"
scope = "narrow"
gate = "auto"
"#;
        let plan: PlanToml = toml::from_str(toml_str).expect("should parse");
        assert_eq!(plan.plan.name, "Test plan");
        assert_eq!(plan.plan.base_branch, "main");
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].name, "task-one");
        assert_eq!(plan.tasks[0].retry_max, 3); // default
        assert!(plan.tasks[0].depends_on.is_empty());
        assert!(plan.tasks[0].invariants.is_empty());
    }

    #[test]
    fn deserialize_full_plan() {
        let toml_str = r#"
[plan]
name = "Add user authentication"
base_branch = "main"

[[tasks]]
name = "implement-jwt-module"
description = """
Implement JWT token generation and validation.
- Create src/auth/jwt.rs
- Implement sign() and verify() functions
- Use RS256 algorithm
"""
scope = "narrow"
gate = "auto"
retry_max = 3
depends_on = []
invariants = ["rust_build", "rust_test", "rust_clippy"]

[[tasks]]
name = "implement-login-endpoint"
description = "Create the /login endpoint."
scope = "medium"
gate = "human_review"
depends_on = ["implement-jwt-module"]
invariants = ["rust_build", "rust_test"]
"#;
        let plan: PlanToml = toml::from_str(toml_str).expect("should parse");
        assert_eq!(plan.plan.name, "Add user authentication");
        assert_eq!(plan.tasks.len(), 2);
        assert_eq!(plan.tasks[0].invariants.len(), 3);
        assert_eq!(plan.tasks[1].depends_on, vec!["implement-jwt-module"]);
    }

    #[test]
    fn deserialize_plan_with_token_budget() {
        let toml_str = r#"
[plan]
name = "Budget plan"
base_branch = "main"
token_budget = 100000

[[tasks]]
name = "task-one"
description = "Do something"
scope = "narrow"
gate = "auto"
"#;
        let plan: PlanToml = toml::from_str(toml_str).expect("should parse");
        assert_eq!(plan.plan.token_budget, Some(100000));
    }

    #[test]
    fn deserialize_plan_without_token_budget() {
        let toml_str = r#"
[plan]
name = "No budget plan"
base_branch = "main"

[[tasks]]
name = "task-one"
description = "Do something"
scope = "narrow"
gate = "auto"
"#;
        let plan: PlanToml = toml::from_str(toml_str).expect("should parse");
        assert_eq!(plan.plan.token_budget, None);
    }

    #[test]
    fn roundtrip_serialize_deserialize() {
        let plan = PlanToml {
            plan: PlanMeta {
                id: None,
                name: "Roundtrip test".to_owned(),
                base_branch: "develop".to_owned(),
                token_budget: None,
                default_harness: "claude-code".to_owned(),
                isolation: "worktree".to_owned(),
                container_image: None,
            },
            tasks: vec![TaskToml {
                name: "t1".to_owned(),
                description: "First task".to_owned(),
                scope: "narrow".to_owned(),
                gate: "auto".to_owned(),
                retry_max: 2,
                depends_on: vec![],
                invariants: vec!["check".to_owned()],
                harness: None,
            }],
        };

        let serialized = toml::to_string(&plan).expect("should serialize");
        let deserialized: PlanToml = toml::from_str(&serialized).expect("should deserialize");
        assert_eq!(plan, deserialized);
    }

    #[test]
    fn deserialize_plan_with_harness_config() {
        let toml_str = r#"
[plan]
name = "Multi-harness plan"
base_branch = "main"
default_harness = "codex-cli"
isolation = "container"

[[tasks]]
name = "task-default"
description = "Uses plan default"
scope = "narrow"
gate = "auto"

[[tasks]]
name = "task-override"
description = "Uses specific harness"
scope = "medium"
gate = "human_review"
harness = "claude-code"
"#;
        let plan: PlanToml = toml::from_str(toml_str).expect("should parse");
        assert_eq!(plan.plan.default_harness, "codex-cli");
        assert_eq!(plan.plan.isolation, "container");
        assert_eq!(plan.tasks[0].harness, None);
        assert_eq!(plan.tasks[1].harness, Some("claude-code".to_owned()));
    }

    #[test]
    fn deserialize_plan_defaults_harness_and_isolation() {
        let toml_str = r#"
[plan]
name = "Defaults plan"
base_branch = "main"

[[tasks]]
name = "task-one"
description = "Do something"
scope = "narrow"
gate = "auto"
"#;
        let plan: PlanToml = toml::from_str(toml_str).expect("should parse");
        assert_eq!(plan.plan.default_harness, "claude-code");
        assert_eq!(plan.plan.isolation, "worktree");
    }

    #[test]
    fn deserialize_plan_with_container_image() {
        let toml_str = r#"
[plan]
name = "Container plan"
base_branch = "main"
isolation = "container"
container_image = "gator-agent:latest"

[[tasks]]
name = "task-one"
description = "Do something"
scope = "narrow"
gate = "auto"
"#;
        let plan: PlanToml = toml::from_str(toml_str).expect("should parse");
        assert_eq!(plan.plan.isolation, "container");
        assert_eq!(
            plan.plan.container_image.as_deref(),
            Some("gator-agent:latest")
        );
    }

    #[test]
    fn deserialize_plan_without_container_image() {
        let toml_str = r#"
[plan]
name = "No image plan"
base_branch = "main"
isolation = "container"

[[tasks]]
name = "task-one"
description = "Do something"
scope = "narrow"
gate = "auto"
"#;
        let plan: PlanToml = toml::from_str(toml_str).expect("should parse");
        assert_eq!(plan.plan.container_image, None);
    }

    /// Helper to resolve a path relative to the workspace root.
    fn workspace_root() -> std::path::PathBuf {
        // CARGO_MANIFEST_DIR is crates/gator-core; go up two levels.
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    }

    #[test]
    fn parse_example_minimal_toml() {
        let path = workspace_root().join("docs/examples/minimal.toml");
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        let plan: PlanToml = toml::from_str(&content)
            .unwrap_or_else(|e| panic!("failed to parse {}: {e}", path.display()));
        assert!(!plan.plan.name.is_empty());
        assert!(!plan.tasks.is_empty());
    }

    #[test]
    fn parse_example_rust_project_toml() {
        let path = workspace_root().join("docs/examples/rust-project.toml");
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        let plan: PlanToml = toml::from_str(&content)
            .unwrap_or_else(|e| panic!("failed to parse {}: {e}", path.display()));
        assert_eq!(plan.plan.name, "Add user authentication");
        assert_eq!(plan.tasks.len(), 4);
        // Verify the diamond DAG structure.
        assert!(
            plan.tasks[0].depends_on.is_empty(),
            "define-types has no deps"
        );
        assert_eq!(plan.tasks[1].depends_on, vec!["define-types"]);
        assert_eq!(plan.tasks[2].depends_on, vec!["define-types"]);
        assert_eq!(plan.tasks[3].depends_on, vec!["impl-jwt", "impl-password"],);
    }
}
