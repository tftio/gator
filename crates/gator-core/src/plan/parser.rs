//! Plan TOML parser with validation.
//!
//! Parses a `plan.toml` string into a [`PlanToml`] and validates:
//! - Scope and gate values are valid enum variants.
//! - Task names are unique.
//! - `depends_on` references point to existing task names.
//! - The dependency graph is acyclic (topological sort).

use std::collections::{HashMap, HashSet, VecDeque};

use gator_db::models::{GatePolicy, ScopeLevel};
use thiserror::Error;

use super::toml_format::PlanToml;

/// Errors that can occur during plan parsing and validation.
#[derive(Debug, Error)]
pub enum PlanParseError {
    #[error("TOML parse error: {0}")]
    TomlError(#[from] toml::de::Error),

    #[error("duplicate task name: {0:?}")]
    DuplicateTaskName(String),

    #[error("task {task:?} depends on unknown task {dependency:?}")]
    UnknownDependency { task: String, dependency: String },

    #[error("invalid scope {value:?} on task {task:?} (expected narrow, medium, or broad)")]
    InvalidScope { task: String, value: String },

    #[error("invalid gate {value:?} on task {task:?} (expected auto, human_review, or human_approve)")]
    InvalidGate { task: String, value: String },

    #[error("dependency cycle detected involving tasks: {0}")]
    CycleDetected(String),

    #[error("plan must contain at least one task")]
    NoTasks,
}

/// Parse and validate a `plan.toml` string.
///
/// Returns a validated [`PlanToml`] or a descriptive error.
pub fn parse_plan_toml(content: &str) -> Result<PlanToml, PlanParseError> {
    let plan: PlanToml = toml::from_str(content)?;
    validate(&plan)?;
    Ok(plan)
}

/// Validate the parsed plan structure.
fn validate(plan: &PlanToml) -> Result<(), PlanParseError> {
    if plan.tasks.is_empty() {
        return Err(PlanParseError::NoTasks);
    }

    // Collect task names and check for duplicates.
    let mut seen = HashSet::new();
    for task in &plan.tasks {
        if !seen.insert(&task.name) {
            return Err(PlanParseError::DuplicateTaskName(task.name.clone()));
        }
    }

    // Validate scope and gate values, and dependency references.
    for task in &plan.tasks {
        // Validate scope.
        if task.scope.parse::<ScopeLevel>().is_err() {
            return Err(PlanParseError::InvalidScope {
                task: task.name.clone(),
                value: task.scope.clone(),
            });
        }

        // Validate gate.
        if task.gate.parse::<GatePolicy>().is_err() {
            return Err(PlanParseError::InvalidGate {
                task: task.name.clone(),
                value: task.gate.clone(),
            });
        }

        // Check dependency references.
        for dep in &task.depends_on {
            if !seen.contains(dep) {
                return Err(PlanParseError::UnknownDependency {
                    task: task.name.clone(),
                    dependency: dep.clone(),
                });
            }
        }
    }

    // Check for cycles using Kahn's algorithm (topological sort).
    check_for_cycles(plan)?;

    Ok(())
}

/// Detect dependency cycles using Kahn's algorithm for topological sort.
///
/// Returns `Ok(())` if the graph is a DAG, or `Err` with details of the cycle.
fn check_for_cycles(plan: &PlanToml) -> Result<(), PlanParseError> {
    // Build adjacency list and in-degree map.
    let task_names: Vec<&str> = plan.tasks.iter().map(|t| t.name.as_str()).collect();
    let name_to_idx: HashMap<&str, usize> = task_names
        .iter()
        .enumerate()
        .map(|(i, name)| (*name, i))
        .collect();

    let n = task_names.len();
    let mut in_degree = vec![0usize; n];
    let mut adj: Vec<Vec<usize>> = vec![vec![]; n];

    for task in &plan.tasks {
        let task_idx = name_to_idx[task.name.as_str()];
        for dep_name in &task.depends_on {
            let dep_idx = name_to_idx[dep_name.as_str()];
            // Edge: dep -> task (dep must complete before task).
            adj[dep_idx].push(task_idx);
            in_degree[task_idx] += 1;
        }
    }

    // Kahn's algorithm.
    let mut queue: VecDeque<usize> = VecDeque::new();
    for (i, deg) in in_degree.iter().enumerate() {
        if *deg == 0 {
            queue.push_back(i);
        }
    }

    let mut sorted_count = 0usize;
    while let Some(node) = queue.pop_front() {
        sorted_count += 1;
        for &neighbor in &adj[node] {
            in_degree[neighbor] -= 1;
            if in_degree[neighbor] == 0 {
                queue.push_back(neighbor);
            }
        }
    }

    if sorted_count != n {
        // Collect the names of tasks that are part of the cycle.
        let cycle_tasks: Vec<&str> = in_degree
            .iter()
            .enumerate()
            .filter(|(_, deg)| **deg > 0)
            .map(|(i, _)| task_names[i])
            .collect();
        return Err(PlanParseError::CycleDetected(cycle_tasks.join(", ")));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_plan() {
        let toml_str = r#"
[plan]
name = "Test"
base_branch = "main"

[[tasks]]
name = "a"
description = "Task A"
scope = "narrow"
gate = "auto"

[[tasks]]
name = "b"
description = "Task B"
scope = "medium"
gate = "human_review"
depends_on = ["a"]
"#;
        let plan = parse_plan_toml(toml_str).expect("should parse");
        assert_eq!(plan.tasks.len(), 2);
    }

    #[test]
    fn rejects_empty_tasks_array() {
        // `tasks = []` is not the same TOML construct as `[[tasks]]`, so the
        // TOML deserializer reports a type error. Either a TomlError or
        // NoTasks is an acceptable rejection.
        let toml_str = r#"
[plan]
name = "Empty"
base_branch = "main"

tasks = []
"#;
        let err = parse_plan_toml(toml_str).unwrap_err();
        assert!(
            matches!(err, PlanParseError::NoTasks | PlanParseError::TomlError(_)),
            "expected NoTasks or TomlError, got: {err}"
        );
    }

    #[test]
    fn rejects_missing_tasks() {
        let toml_str = r#"
[plan]
name = "No tasks"
base_branch = "main"
"#;
        let err = parse_plan_toml(toml_str).unwrap_err();
        assert!(
            matches!(err, PlanParseError::NoTasks),
            "expected NoTasks, got: {err}"
        );
    }

    #[test]
    fn rejects_duplicate_task_names() {
        let toml_str = r#"
[plan]
name = "Dup"
base_branch = "main"

[[tasks]]
name = "a"
description = "First A"
scope = "narrow"
gate = "auto"

[[tasks]]
name = "a"
description = "Second A"
scope = "narrow"
gate = "auto"
"#;
        let err = parse_plan_toml(toml_str).unwrap_err();
        assert!(
            matches!(err, PlanParseError::DuplicateTaskName(ref n) if n == "a"),
            "expected DuplicateTaskName, got: {err}"
        );
    }

    #[test]
    fn rejects_unknown_dependency() {
        let toml_str = r#"
[plan]
name = "Bad dep"
base_branch = "main"

[[tasks]]
name = "a"
description = "Task A"
scope = "narrow"
gate = "auto"
depends_on = ["nonexistent"]
"#;
        let err = parse_plan_toml(toml_str).unwrap_err();
        assert!(
            matches!(err, PlanParseError::UnknownDependency { .. }),
            "expected UnknownDependency, got: {err}"
        );
    }

    #[test]
    fn rejects_invalid_scope() {
        let toml_str = r#"
[plan]
name = "Bad scope"
base_branch = "main"

[[tasks]]
name = "a"
description = "Task A"
scope = "tiny"
gate = "auto"
"#;
        let err = parse_plan_toml(toml_str).unwrap_err();
        assert!(
            matches!(err, PlanParseError::InvalidScope { .. }),
            "expected InvalidScope, got: {err}"
        );
    }

    #[test]
    fn rejects_invalid_gate() {
        let toml_str = r#"
[plan]
name = "Bad gate"
base_branch = "main"

[[tasks]]
name = "a"
description = "Task A"
scope = "narrow"
gate = "robot"
"#;
        let err = parse_plan_toml(toml_str).unwrap_err();
        assert!(
            matches!(err, PlanParseError::InvalidGate { .. }),
            "expected InvalidGate, got: {err}"
        );
    }

    #[test]
    fn rejects_direct_cycle() {
        let toml_str = r#"
[plan]
name = "Cycle"
base_branch = "main"

[[tasks]]
name = "a"
description = "Task A"
scope = "narrow"
gate = "auto"
depends_on = ["b"]

[[tasks]]
name = "b"
description = "Task B"
scope = "narrow"
gate = "auto"
depends_on = ["a"]
"#;
        let err = parse_plan_toml(toml_str).unwrap_err();
        assert!(
            matches!(err, PlanParseError::CycleDetected(_)),
            "expected CycleDetected, got: {err}"
        );
    }

    #[test]
    fn rejects_transitive_cycle() {
        let toml_str = r#"
[plan]
name = "Transitive Cycle"
base_branch = "main"

[[tasks]]
name = "a"
description = "Task A"
scope = "narrow"
gate = "auto"
depends_on = ["c"]

[[tasks]]
name = "b"
description = "Task B"
scope = "narrow"
gate = "auto"
depends_on = ["a"]

[[tasks]]
name = "c"
description = "Task C"
scope = "narrow"
gate = "auto"
depends_on = ["b"]
"#;
        let err = parse_plan_toml(toml_str).unwrap_err();
        assert!(
            matches!(err, PlanParseError::CycleDetected(_)),
            "expected CycleDetected, got: {err}"
        );
    }

    #[test]
    fn accepts_complex_dag() {
        // Diamond dependency: a -> b, a -> c, b -> d, c -> d
        let toml_str = r#"
[plan]
name = "Diamond"
base_branch = "main"

[[tasks]]
name = "a"
description = "Task A"
scope = "narrow"
gate = "auto"

[[tasks]]
name = "b"
description = "Task B"
scope = "narrow"
gate = "auto"
depends_on = ["a"]

[[tasks]]
name = "c"
description = "Task C"
scope = "narrow"
gate = "auto"
depends_on = ["a"]

[[tasks]]
name = "d"
description = "Task D"
scope = "broad"
gate = "human_approve"
depends_on = ["b", "c"]
"#;
        let plan = parse_plan_toml(toml_str).expect("diamond DAG should be valid");
        assert_eq!(plan.tasks.len(), 4);
    }

    #[test]
    fn rejects_malformed_toml() {
        let err = parse_plan_toml("this is not valid toml {{{").unwrap_err();
        assert!(
            matches!(err, PlanParseError::TomlError(_)),
            "expected TomlError, got: {err}"
        );
    }

    #[test]
    fn all_scope_values_accepted() {
        for scope in &["narrow", "medium", "broad"] {
            let toml_str = format!(
                r#"
[plan]
name = "Scope test"
base_branch = "main"

[[tasks]]
name = "t"
description = "test"
scope = "{scope}"
gate = "auto"
"#
            );
            parse_plan_toml(&toml_str)
                .unwrap_or_else(|e| panic!("scope {scope:?} should be valid: {e}"));
        }
    }

    #[test]
    fn all_gate_values_accepted() {
        for gate in &["auto", "human_review", "human_approve"] {
            let toml_str = format!(
                r#"
[plan]
name = "Gate test"
base_branch = "main"

[[tasks]]
name = "t"
description = "test"
scope = "narrow"
gate = "{gate}"
"#
            );
            parse_plan_toml(&toml_str)
                .unwrap_or_else(|e| panic!("gate {gate:?} should be valid: {e}"));
        }
    }
}
