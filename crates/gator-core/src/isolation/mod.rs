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
    /// Filesystem path to the workspace the agent sees.
    ///
    /// For worktree isolation this is the host worktree path. For container
    /// isolation this is `/workspace` inside the container.
    pub path: PathBuf,
    /// Host-side worktree path (set only for container isolation).
    ///
    /// When running in a sandboxed container the agent writes to a
    /// container-local filesystem. After the agent finishes, results are
    /// extracted back to this host path for gate checks and commits.
    pub host_path: Option<PathBuf>,
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

    /// Extract results from the workspace back to the host filesystem.
    ///
    /// For worktree isolation this is a no-op (the agent already wrote to the
    /// host worktree). For container isolation this copies files from the
    /// container's `/workspace` back to the host worktree, excluding `.git`.
    async fn extract_results(&self, info: &WorkspaceInfo) -> Result<()>;

    /// Remove a previously created workspace.
    async fn remove_workspace(&self, info: &WorkspaceInfo) -> Result<()>;
}

/// Factory function: create an isolation backend from a mode string.
///
/// `container_image` is only used when `mode` is `"container"`. It defaults
/// to `"ubuntu:24.04"` when `None`.
pub fn create_isolation(
    mode: &str,
    repo_path: &Path,
    container_image: Option<&str>,
) -> Result<Arc<dyn Isolation>> {
    match mode {
        "worktree" => {
            let mgr = crate::worktree::WorktreeManager::new(repo_path, None)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(Arc::new(worktree::WorktreeIsolation::new(mgr)))
        }
        "container" => {
            let image = container_image.unwrap_or("ubuntu:24.04").to_string();
            let mgr = crate::worktree::WorktreeManager::new(repo_path, None)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let config = container::ContainerConfig {
                image,
                extra_flags: vec![],
            };
            Ok(Arc::new(container::ContainerIsolation::new(config, mgr)))
        }
        other => {
            bail!("unknown isolation mode: {other:?} (expected \"worktree\" or \"container\")")
        }
    }
}
