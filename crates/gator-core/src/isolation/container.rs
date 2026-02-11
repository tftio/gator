//! Docker container isolation backend (sandboxed).
//!
//! Creates Docker containers **without** bind-mounting the host repository.
//! The host worktree contents are copied into the container via `docker cp`,
//! and after the agent finishes, results are extracted back out via `docker cp`.
//! This ensures the agent cannot write to the host filesystem directly.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use tokio::process::Command;

use super::{Isolation, WorkspaceInfo};
use crate::worktree::WorktreeManager;

/// Configuration for the container isolation backend.
#[derive(Debug, Clone)]
pub struct ContainerConfig {
    /// Docker image to use (e.g. "gator-agent:latest").
    pub image: String,
    /// Additional flags to pass to `docker create`.
    pub extra_flags: Vec<String>,
}

/// Isolation backend that runs tasks inside sandboxed Docker containers.
///
/// The agent writes to a container-local `/workspace` directory. No host
/// paths are bind-mounted read-write. Results are extracted via `docker cp`
/// after the agent completes.
#[derive(Debug)]
pub struct ContainerIsolation {
    config: ContainerConfig,
    worktree_manager: WorktreeManager,
}

impl ContainerIsolation {
    /// Create a new container isolation backend.
    pub fn new(config: ContainerConfig, worktree_manager: WorktreeManager) -> Self {
        Self {
            config,
            worktree_manager,
        }
    }

    /// Build the container name for a plan/task pair.
    fn container_name(plan_name: &str, task_name: &str) -> String {
        // Sanitize names for Docker container naming (alphanumeric + hyphens).
        let sanitize = |s: &str| -> String {
            s.chars()
                .map(|c| {
                    if c.is_alphanumeric() || c == '-' {
                        c
                    } else {
                        '-'
                    }
                })
                .collect()
        };
        format!("gator-{}-{}", sanitize(plan_name), sanitize(task_name))
    }

    /// Build the branch name for a plan/task pair (same convention as worktree).
    fn branch_name(plan_name: &str, task_name: &str) -> String {
        WorktreeManager::branch_name(plan_name, task_name)
    }

    /// Copy files from host path into the container, excluding `.git`.
    ///
    /// Uses a tar pipe to exclude `.git` during the copy:
    ///   tar -C <host_path> --exclude='.git' -cf - . | docker cp - <cid>:/workspace
    async fn copy_into_container(container_id: &str, host_path: &std::path::Path) -> Result<()> {
        // First create the /workspace directory inside the container.
        let mkdir_output = Command::new("docker")
            .args(["exec", container_id, "mkdir", "-p", "/workspace"])
            .output()
            .await
            .context("failed to run docker exec mkdir")?;

        if !mkdir_output.status.success() {
            let stderr = String::from_utf8_lossy(&mkdir_output.stderr);
            bail!("docker exec mkdir -p /workspace failed: {stderr}");
        }

        // Use tar pipe to copy contents excluding .git:
        //   tar -C <host_path> --exclude='.git' -cf - . | docker cp - <cid>:/workspace
        let tar_cmd = format!(
            "tar -C {} --exclude='.git' -cf - . | docker cp - {}:/workspace",
            shell_escape(host_path),
            container_id,
        );

        let output = Command::new("sh")
            .args(["-c", &tar_cmd])
            .output()
            .await
            .context("failed to copy files into container")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("copy into container failed: {stderr}");
        }

        Ok(())
    }

    /// Copy files from the container back to a host directory, excluding `.git`.
    ///
    /// Uses a tar pipe:
    ///   docker cp <cid>:/workspace/. - | tar -C <dest> --exclude='.git' -xf -
    async fn copy_from_container(container_id: &str, dest_path: &std::path::Path) -> Result<()> {
        let tar_cmd = format!(
            "docker cp {}:/workspace/. - | tar -C {} --exclude='.git' -xf -",
            container_id,
            shell_escape(dest_path),
        );

        let output = Command::new("sh")
            .args(["-c", &tar_cmd])
            .output()
            .await
            .context("failed to copy files from container")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("copy from container failed: {stderr}");
        }

        Ok(())
    }
}

/// Escape a path for use in a shell command.
fn shell_escape(path: &std::path::Path) -> String {
    let s = path.to_string_lossy();
    // Wrap in single quotes, escaping any embedded single quotes.
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[async_trait]
impl Isolation for ContainerIsolation {
    fn name(&self) -> &str {
        "container"
    }

    async fn create_workspace(&self, plan_name: &str, task_name: &str) -> Result<WorkspaceInfo> {
        let container_name = Self::container_name(plan_name, task_name);
        let branch_name = Self::branch_name(plan_name, task_name);

        // 1. Create a host worktree via WorktreeManager.
        let wt_info = self
            .worktree_manager
            .create_worktree(&branch_name)
            .map_err(|e| anyhow::anyhow!("{e}"))
            .with_context(|| format!("failed to create worktree for {plan_name}/{task_name}"))?;

        let host_worktree_path = wt_info.path.clone();

        // 2. docker create WITHOUT volume mount.
        let mut args = vec![
            "create".to_string(),
            "--name".to_string(),
            container_name.clone(),
            "-w".to_string(),
            "/workspace".to_string(),
        ];

        for flag in &self.config.extra_flags {
            args.push(flag.clone());
        }

        args.push(self.config.image.clone());
        args.push("sleep".to_string());
        args.push("infinity".to_string());

        let output = Command::new("docker")
            .args(&args)
            .output()
            .await
            .context("failed to run docker create")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("docker create failed: {stderr}");
        }

        let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();

        // 3. docker start.
        let start_output = Command::new("docker")
            .args(["start", &container_id])
            .output()
            .await
            .context("failed to run docker start")?;

        if !start_output.status.success() {
            let stderr = String::from_utf8_lossy(&start_output.stderr);
            // Clean up the created container.
            let _ = Command::new("docker")
                .args(["rm", "-f", &container_id])
                .output()
                .await;
            bail!("docker start failed: {stderr}");
        }

        // 4. Copy worktree contents into the container (excluding .git).
        if let Err(e) = Self::copy_into_container(&container_id, &host_worktree_path).await {
            // Clean up container on failure.
            let _ = Command::new("docker")
                .args(["rm", "-f", &container_id])
                .output()
                .await;
            return Err(e.context("failed to copy worktree into container"));
        }

        Ok(WorkspaceInfo {
            path: PathBuf::from("/workspace"),
            host_path: Some(host_worktree_path),
            branch: wt_info.branch,
            container_id: Some(container_id),
        })
    }

    async fn extract_results(&self, info: &WorkspaceInfo) -> Result<()> {
        let container_id = info
            .container_id
            .as_deref()
            .context("extract_results called without container_id")?;
        let host_path = info
            .host_path
            .as_deref()
            .context("extract_results called without host_path")?;

        tracing::info!(
            container_id = container_id,
            host_path = %host_path.display(),
            "extracting results from container to host worktree"
        );

        Self::copy_from_container(container_id, host_path).await
    }

    async fn remove_workspace(&self, info: &WorkspaceInfo) -> Result<()> {
        // Remove the Docker container.
        if let Some(ref container_id) = info.container_id {
            let output = Command::new("docker")
                .args(["rm", "-f", container_id])
                .output()
                .await
                .context("failed to run docker rm")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                // Don't fail if container is already gone.
                if !stderr.contains("No such container") {
                    bail!("docker rm -f {container_id} failed: {stderr}");
                }
            }
        }

        // Remove the host worktree.
        if let Some(ref host_path) = info.host_path {
            self.worktree_manager
                .remove_worktree(host_path)
                .map_err(|e| anyhow::anyhow!("{e}"))
                .with_context(|| format!("failed to remove worktree at {}", host_path.display()))?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_name_sanitizes() {
        assert_eq!(
            ContainerIsolation::container_name("my plan", "task/one"),
            "gator-my-plan-task-one"
        );
        assert_eq!(
            ContainerIsolation::container_name("alpha", "beta"),
            "gator-alpha-beta"
        );
    }

    #[test]
    fn branch_name_format() {
        assert_eq!(
            ContainerIsolation::branch_name("plan-1", "task-a"),
            "gator/plan-1/task-a"
        );
    }

    #[test]
    fn container_isolation_name() {
        use std::process::Command;
        use tempfile::TempDir;

        // Create a temporary git repo so WorktreeManager::new succeeds.
        let dir = TempDir::new().expect("failed to create temp dir");
        let repo_path = dir.path().to_path_buf();
        let run = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(&repo_path)
                .output()
                .unwrap();
            assert!(output.status.success());
        };
        run(&["init"]);
        run(&["config", "user.email", "test@gator.dev"]);
        run(&["config", "user.name", "Gator Test"]);
        std::fs::write(repo_path.join("README.md"), "# Test\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "Initial commit"]);

        let mgr = WorktreeManager::new(&repo_path, None).unwrap();
        let config = ContainerConfig {
            image: "ubuntu:24.04".to_string(),
            extra_flags: vec![],
        };
        let iso = ContainerIsolation::new(config, mgr);
        assert_eq!(iso.name(), "container");
    }

    #[test]
    fn shell_escape_simple_path() {
        let path = std::path::Path::new("/tmp/my-worktree");
        assert_eq!(shell_escape(path), "'/tmp/my-worktree'");
    }

    #[test]
    fn shell_escape_path_with_single_quote() {
        let path = std::path::Path::new("/tmp/it's-a-test");
        assert_eq!(shell_escape(path), "'/tmp/it'\\''s-a-test'");
    }
}
