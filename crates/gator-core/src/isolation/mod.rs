//! Workspace isolation abstraction.
//!
//! Decouples workspace creation from `WorktreeManager` so that different
//! backends (git worktrees, Docker containers) can be used interchangeably.

pub mod container;
pub mod worktree;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;

/// Information about a created workspace.
#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    /// Filesystem path to the workspace (worktree dir or mounted volume path).
    pub path: PathBuf,
    /// Git branch name, if applicable.
    pub branch: Option<String>,
    /// Docker container ID, if applicable.
    pub container_id: Option<String>,
}

/// Trait for workspace isolation backends.
#[async_trait]
pub trait Isolation: Send + Sync {
    /// Human-readable name of the isolation backend (e.g. "worktree", "container").
    fn name(&self) -> &str;

    /// Create an isolated workspace for a task.
    async fn create_workspace(&self, plan_name: &str, task_name: &str) -> Result<WorkspaceInfo>;

    /// Remove a previously created workspace.
    async fn remove_workspace(&self, info: &WorkspaceInfo) -> Result<()>;
}

/// Factory function: create an isolation backend from a mode string.
pub fn create_isolation(mode: &str, repo_path: &Path) -> Result<Arc<dyn Isolation>> {
    match mode {
        "worktree" => {
            let mgr = crate::worktree::WorktreeManager::new(repo_path, None)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(Arc::new(worktree::WorktreeIsolation::new(mgr)))
        }
        "container" => {
            let config = container::ContainerConfig {
                image: "ubuntu:24.04".to_string(),
                repo_path: repo_path.to_path_buf(),
                extra_flags: vec![],
            };
            Ok(Arc::new(container::ContainerIsolation::new(config)))
        }
        other => {
            bail!("unknown isolation mode: {other:?} (expected \"worktree\" or \"container\")")
        }
    }
}
