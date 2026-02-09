use gator_db::models::{InvariantKind, InvariantScope};

/// Domain-level definition of an invariant, used to create new invariants.
///
/// Use [`InvariantDefinition::new`] for the required fields, then chain
/// optional setters (builder-style) before passing to the DB layer.
#[derive(Debug, Clone)]
pub struct InvariantDefinition {
    /// Unique human-readable name (e.g. `rust_build`).
    pub name: String,
    /// Optional longer description.
    pub description: Option<String>,
    /// Category of the invariant.
    pub kind: InvariantKind,
    /// The executable to run (e.g. `cargo`).
    pub command: String,
    /// Arguments passed to the command.
    pub args: Vec<String>,
    /// The process exit code that means "pass". Defaults to `0`.
    pub expected_exit_code: i32,
    /// Optional numeric threshold (e.g. coverage percentage).
    pub threshold: Option<f32>,
    /// Whether this invariant applies globally or per-project.
    pub scope: InvariantScope,
}

impl InvariantDefinition {
    /// Create a new definition with the required fields.
    ///
    /// Optional fields are set to their defaults:
    /// - `description`: `None`
    /// - `args`: empty
    /// - `expected_exit_code`: `0`
    /// - `threshold`: `None`
    /// - `scope`: `Project`
    pub fn new(name: impl Into<String>, kind: InvariantKind, command: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            kind,
            command: command.into(),
            args: Vec::new(),
            expected_exit_code: 0,
            threshold: None,
            scope: InvariantScope::Project,
        }
    }

    /// Set the description.
    pub fn description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    /// Set command arguments.
    pub fn args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }

    /// Set the expected exit code.
    pub fn expected_exit_code(mut self, code: i32) -> Self {
        self.expected_exit_code = code;
        self
    }

    /// Set the threshold value.
    pub fn threshold(mut self, val: f32) -> Self {
        self.threshold = Some(val);
        self
    }

    /// Set the scope.
    pub fn scope(mut self, scope: InvariantScope) -> Self {
        self.scope = scope;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_sets_defaults() {
        let def = InvariantDefinition::new("rust_build", InvariantKind::Typecheck, "cargo");
        assert_eq!(def.name, "rust_build");
        assert_eq!(def.kind, InvariantKind::Typecheck);
        assert_eq!(def.command, "cargo");
        assert!(def.args.is_empty());
        assert_eq!(def.expected_exit_code, 0);
        assert!(def.description.is_none());
        assert!(def.threshold.is_none());
        assert_eq!(def.scope, InvariantScope::Project);
    }

    #[test]
    fn builder_sets_optional_fields() {
        let def = InvariantDefinition::new("coverage_check", InvariantKind::Coverage, "cargo")
            .description("Check code coverage meets threshold")
            .args(vec!["tarpaulin".into(), "--workspace".into()])
            .expected_exit_code(0)
            .threshold(80.0)
            .scope(InvariantScope::Global);

        assert_eq!(
            def.description.as_deref(),
            Some("Check code coverage meets threshold")
        );
        assert_eq!(def.args, vec!["tarpaulin", "--workspace"]);
        assert_eq!(def.expected_exit_code, 0);
        assert_eq!(def.threshold, Some(80.0));
        assert_eq!(def.scope, InvariantScope::Global);
    }
}
