//! Git worktree isolation backend.
//!
//! Wraps [`WorktreeManager`] behind the [`Isolation`] trait.

use anyhow::{Context, Result};
use async_trait::async_trait;

use super::{Isolation, WorkspaceInfo};
use crate::worktree::WorktreeManager;

/// Isolation backend backed by git worktrees.
#[derive(Debug)]
pub struct WorktreeIsolation {
    manager: WorktreeManager,
}

impl WorktreeIsolation {
    /// Create a new `WorktreeIsolation` from an existing `WorktreeManager`.
    pub fn new(manager: WorktreeManager) -> Self {
        Self { manager }
    }

    /// Access the underlying `WorktreeManager`.
    pub fn manager(&self) -> &WorktreeManager {
        &self.manager
    }
}

#[async_trait]
impl Isolation for WorktreeIsolation {
    fn name(&self) -> &str {
        "worktree"
    }

    async fn create_workspace(&self, plan_name: &str, task_name: &str) -> Result<WorkspaceInfo> {
        let branch_name = WorktreeManager::branch_name(plan_name, task_name);
        let wt_info = self
            .manager
            .create_worktree(&branch_name)
            .with_context(|| format!("failed to create worktree for {plan_name}/{task_name}"))?;

        Ok(WorkspaceInfo {
            path: wt_info.path,
            host_path: None,
            branch: wt_info.branch,
            container_id: None,
        })
    }

    async fn extract_results(&self, _info: &WorkspaceInfo) -> Result<()> {
        // No-op: worktree isolation writes directly to the host filesystem.
        Ok(())
    }

    async fn remove_workspace(&self, info: &WorkspaceInfo) -> Result<()> {
        self.manager
            .remove_worktree(&info.path)
            .with_context(|| format!("failed to remove worktree at {}", info.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    /// Create a temporary git repo for testing.
    fn create_temp_repo() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().expect("failed to create temp dir");
        let repo_path = dir.path().to_path_buf();

        let run = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(&repo_path)
                .output()
                .unwrap_or_else(|e| panic!("git {} failed: {e}", args.join(" ")));
            assert!(output.status.success(), "git {} failed", args.join(" "));
        };

        run(&["init"]);
        run(&["config", "user.email", "test@gator.dev"]);
        run(&["config", "user.name", "Gator Test"]);
        std::fs::write(repo_path.join("README.md"), "# Test\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "Initial commit"]);

        (dir, repo_path)
    }

    #[tokio::test]
    async fn worktree_isolation_create_and_remove() {
        let (_dir, repo_path) = create_temp_repo();
        let wt_base = TempDir::new().unwrap();
        let mgr = WorktreeManager::new(&repo_path, Some(wt_base.path().to_path_buf())).unwrap();
        let isolation = WorktreeIsolation::new(mgr);

        assert_eq!(isolation.name(), "worktree");

        let info = isolation
            .create_workspace("test-plan", "test-task")
            .await
            .expect("create_workspace failed");

        assert!(info.path.exists());
        assert_eq!(info.branch.as_deref(), Some("gator/test-plan/test-task"));
        assert!(info.host_path.is_none());
        assert!(info.container_id.is_none());

        isolation
            .remove_workspace(&info)
            .await
            .expect("remove_workspace failed");

        assert!(!info.path.exists());
    }
}
