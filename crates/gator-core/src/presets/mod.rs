//! Invariant preset library and project detection.
//!
//! Provides a built-in library of standard invariant definitions for common
//! project types (Rust, Node, Python, Go). The presets are defined in
//! `invariants.toml` and embedded in the binary at compile time.

use std::path::Path;
use std::process::Command;

use serde::Deserialize;

/// A single invariant preset from the embedded library.
#[derive(Debug, Clone, Deserialize)]
pub struct InvariantPreset {
    /// Unique invariant name (e.g. `rust_build`).
    pub name: String,
    /// Project type this preset belongs to (e.g. `rust`, `node`).
    pub project_type: String,
    /// Human-readable description of what the invariant checks.
    pub description: String,
    /// Kind of invariant: `test_suite`, `typecheck`, `lint`, `coverage`, `custom`.
    pub kind: String,
    /// Command to execute (e.g. `cargo`, `npm`).
    pub command: String,
    /// Arguments to pass to the command.
    pub args: Vec<String>,
}

/// Container for deserializing the embedded TOML file.
#[derive(Debug, Deserialize)]
struct PresetLibrary {
    presets: Vec<InvariantPreset>,
}

/// The embedded invariant presets TOML.
static PRESETS_TOML: &str = include_str!("invariants.toml");

/// Load all invariant presets from the embedded library.
///
/// # Panics
///
/// Panics if the embedded TOML is malformed. This is a compile-time invariant
/// -- if the binary was built, the TOML is valid.
pub fn load_presets() -> Vec<InvariantPreset> {
    let lib: PresetLibrary =
        toml::from_str(PRESETS_TOML).expect("embedded invariants.toml is invalid");
    lib.presets
}

/// Return presets matching a given project type.
pub fn presets_for_project_type(project_type: &str) -> Vec<InvariantPreset> {
    load_presets()
        .into_iter()
        .filter(|p| p.project_type == project_type)
        .collect()
}

/// Return the list of distinct project types defined in the preset library.
pub fn available_project_types() -> Vec<String> {
    let presets = load_presets();
    let mut types: Vec<String> = presets.iter().map(|p| p.project_type.clone()).collect();
    types.sort();
    types.dedup();
    types
}

/// Detect the project type by looking for marker files in `dir`.
///
/// Returns `None` if no recognized project type is found.
pub fn detect_project_type(dir: &Path) -> Option<String> {
    if dir.join("Cargo.toml").exists() {
        Some("rust".to_string())
    } else if dir.join("package.json").exists() {
        Some("node".to_string())
    } else if dir.join("pyproject.toml").exists() || dir.join("setup.py").exists() {
        Some("python".to_string())
    } else if dir.join("go.mod").exists() {
        Some("go".to_string())
    } else {
        None
    }
}

/// Detect the base branch for the git repository at `dir`.
///
/// Tries `git symbolic-ref refs/remotes/origin/HEAD` first, falls back to
/// the current branch, and ultimately defaults to `"main"`.
pub fn detect_base_branch(dir: &Path) -> String {
    // Try: remote HEAD reference (e.g. "refs/remotes/origin/main" -> "main")
    if let Ok(output) = Command::new("git")
        .args(["symbolic-ref", "refs/remotes/origin/HEAD"])
        .current_dir(dir)
        .output()
    {
        if output.status.success() {
            let refname = String::from_utf8_lossy(&output.stdout);
            let refname = refname.trim();
            // Strip "refs/remotes/origin/" prefix.
            if let Some(branch) = refname.strip_prefix("refs/remotes/origin/") {
                if !branch.is_empty() {
                    return branch.to_string();
                }
            }
        }
    }

    // Fallback: current branch name.
    if let Ok(output) = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(dir)
        .output()
    {
        if output.status.success() {
            let branch = String::from_utf8_lossy(&output.stdout);
            let branch = branch.trim();
            if !branch.is_empty() {
                return branch.to_string();
            }
        }
    }

    // Final fallback.
    "main".to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_presets_returns_nonempty() {
        let presets = load_presets();
        assert!(
            !presets.is_empty(),
            "embedded preset library should not be empty"
        );
    }

    #[test]
    fn presets_for_rust() {
        let presets = presets_for_project_type("rust");
        assert!(
            presets.len() >= 4,
            "expected at least 4 rust presets, got {}",
            presets.len()
        );
        let names: Vec<&str> = presets.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"rust_build"));
        assert!(names.contains(&"rust_test"));
        assert!(names.contains(&"rust_clippy"));
        assert!(names.contains(&"rust_fmt_check"));
    }

    #[test]
    fn presets_for_node() {
        let presets = presets_for_project_type("node");
        assert!(
            presets.len() >= 3,
            "expected at least 3 node presets, got {}",
            presets.len()
        );
    }

    #[test]
    fn presets_for_python() {
        let presets = presets_for_project_type("python");
        assert!(
            presets.len() >= 3,
            "expected at least 3 python presets, got {}",
            presets.len()
        );
    }

    #[test]
    fn presets_for_go() {
        let presets = presets_for_project_type("go");
        assert!(
            presets.len() >= 3,
            "expected at least 3 go presets, got {}",
            presets.len()
        );
    }

    #[test]
    fn presets_for_nonexistent_returns_empty() {
        let presets = presets_for_project_type("nonexistent");
        assert!(presets.is_empty());
    }

    #[test]
    fn available_types_includes_all() {
        let types = available_project_types();
        assert!(types.contains(&"rust".to_string()));
        assert!(types.contains(&"node".to_string()));
        assert!(types.contains(&"python".to_string()));
        assert!(types.contains(&"go".to_string()));
    }

    #[test]
    fn all_preset_kinds_are_valid() {
        let valid_kinds = ["test_suite", "typecheck", "lint", "coverage", "custom"];
        for preset in &load_presets() {
            assert!(
                valid_kinds.contains(&preset.kind.as_str()),
                "preset {:?} has invalid kind {:?}",
                preset.name,
                preset.kind
            );
        }
    }

    #[test]
    fn all_preset_names_are_unique() {
        let presets = load_presets();
        let mut names: Vec<&str> = presets.iter().map(|p| p.name.as_str()).collect();
        let original_len = names.len();
        names.sort();
        names.dedup();
        assert_eq!(
            names.len(),
            original_len,
            "preset names must be unique across all project types"
        );
    }

    #[test]
    fn detect_rust_project() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        assert_eq!(detect_project_type(dir.path()), Some("rust".to_string()));
    }

    #[test]
    fn detect_node_project() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("package.json"), "").unwrap();
        assert_eq!(detect_project_type(dir.path()), Some("node".to_string()));
    }

    #[test]
    fn detect_python_pyproject() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("pyproject.toml"), "").unwrap();
        assert_eq!(detect_project_type(dir.path()), Some("python".to_string()));
    }

    #[test]
    fn detect_python_setup_py() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("setup.py"), "").unwrap();
        assert_eq!(detect_project_type(dir.path()), Some("python".to_string()));
    }

    #[test]
    fn detect_go_project() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("go.mod"), "").unwrap();
        assert_eq!(detect_project_type(dir.path()), Some("go".to_string()));
    }

    #[test]
    fn detect_unknown_project() {
        let dir = TempDir::new().unwrap();
        assert_eq!(detect_project_type(dir.path()), None);
    }

    #[test]
    fn detect_base_branch_fallback() {
        // In a temp dir with no git repo, should fall back to "main".
        let dir = TempDir::new().unwrap();
        let branch = detect_base_branch(dir.path());
        assert_eq!(branch, "main");
    }

    #[test]
    fn rust_presets_have_priority_order() {
        // Verify the presets are returned in the order defined in the TOML
        // (build, test, clippy, fmt_check).
        let presets = presets_for_project_type("rust");
        let names: Vec<&str> = presets.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names[0], "rust_build");
        assert_eq!(names[1], "rust_test");
        assert_eq!(names[2], "rust_clippy");
        assert_eq!(names[3], "rust_fmt_check");
    }
}
