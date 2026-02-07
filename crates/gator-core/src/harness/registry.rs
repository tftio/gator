//! Harness registry -- a named collection of available harness adapters.
//!
//! The registry allows the orchestrator to look up harnesses by name at
//! runtime (e.g. when a task specifies `assigned_harness = "claude-code"`).

use std::collections::HashMap;

use super::trait_def::Harness;

/// A collection of registered [`Harness`] implementations, keyed by name.
///
/// # Example
///
/// ```ignore
/// let mut registry = HarnessRegistry::new();
/// registry.register(ClaudeCodeAdapter::new());
/// let harness = registry.get("claude-code").unwrap();
/// ```
#[derive(Default)]
pub struct HarnessRegistry {
    harnesses: HashMap<String, Box<dyn Harness>>,
}

impl HarnessRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a harness adapter.
    ///
    /// The harness is stored under the name returned by [`Harness::name`].
    /// If a harness with the same name is already registered, it is
    /// replaced and the old one is returned.
    pub fn register(&mut self, harness: impl Harness + 'static) -> Option<Box<dyn Harness>> {
        let name = harness.name().to_string();
        self.harnesses.insert(name, Box::new(harness))
    }

    /// Look up a harness by name.
    pub fn get(&self, name: &str) -> Option<&dyn Harness> {
        self.harnesses.get(name).map(|b| b.as_ref())
    }

    /// List the names of all registered harnesses.
    ///
    /// The order is not guaranteed (HashMap iteration order).
    pub fn list(&self) -> Vec<&str> {
        self.harnesses.keys().map(|s| s.as_str()).collect()
    }

    /// Return the number of registered harnesses.
    pub fn len(&self) -> usize {
        self.harnesses.len()
    }

    /// Return `true` if no harnesses are registered.
    pub fn is_empty(&self) -> bool {
        self.harnesses.is_empty()
    }
}

impl std::fmt::Debug for HarnessRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HarnessRegistry")
            .field("harnesses", &self.harnesses.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::types::{AgentEvent, AgentHandle, MaterializedTask};
    use anyhow::Result;
    use async_trait::async_trait;
    use futures::Stream;
    use std::pin::Pin;
    use uuid::Uuid;

    /// Minimal test harness.
    struct FakeHarness {
        harness_name: String,
    }

    impl FakeHarness {
        fn new(name: &str) -> Self {
            Self {
                harness_name: name.to_string(),
            }
        }
    }

    #[async_trait]
    impl Harness for FakeHarness {
        fn name(&self) -> &str {
            &self.harness_name
        }

        async fn spawn(&self, _task: &MaterializedTask) -> Result<AgentHandle> {
            Ok(AgentHandle {
                pid: 42,
                stdin: None,
                task_id: Uuid::nil(),
                attempt: 0,
                harness_name: self.harness_name.clone(),
            })
        }

        fn events(&self, _handle: &AgentHandle) -> Pin<Box<dyn Stream<Item = AgentEvent> + Send>> {
            Box::pin(futures::stream::empty())
        }

        async fn send(&self, _handle: &AgentHandle, _message: &str) -> Result<()> {
            Ok(())
        }

        async fn kill(&self, _handle: &AgentHandle) -> Result<()> {
            Ok(())
        }

        async fn is_running(&self, _handle: &AgentHandle) -> bool {
            false
        }
    }

    #[test]
    fn registry_starts_empty() {
        let registry = HarnessRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.list().is_empty());
    }

    #[test]
    fn register_and_get() {
        let mut registry = HarnessRegistry::new();
        let old = registry.register(FakeHarness::new("alpha"));
        assert!(old.is_none());

        let harness = registry.get("alpha");
        assert!(harness.is_some());
        assert_eq!(harness.unwrap().name(), "alpha");
    }

    #[test]
    fn register_replaces_existing() {
        let mut registry = HarnessRegistry::new();
        registry.register(FakeHarness::new("alpha"));
        let old = registry.register(FakeHarness::new("alpha"));
        assert!(old.is_some());
        assert_eq!(old.unwrap().name(), "alpha");
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn get_missing_returns_none() {
        let registry = HarnessRegistry::new();
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn list_returns_all_names() {
        let mut registry = HarnessRegistry::new();
        registry.register(FakeHarness::new("alpha"));
        registry.register(FakeHarness::new("beta"));
        registry.register(FakeHarness::new("gamma"));

        let mut names = registry.list();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn registry_debug_shows_names() {
        let mut registry = HarnessRegistry::new();
        registry.register(FakeHarness::new("test-harness"));
        let debug = format!("{registry:?}");
        assert!(debug.contains("test-harness"));
    }
}
