//! Docker container isolation backend.
//!
//! Creates Docker containers with the repository mounted as a volume.
//! Each task gets its own container for filesystem isolation.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use tokio::process::Command;

use super::{Isolation, WorkspaceInfo};

/// Configuration for the container isolation backend.
#[derive(Debug, Clone)]
pub struct ContainerConfig {
    /// Docker image to use (e.g. "ubuntu:24.04").
    pub image: String,
    /// Path to the host repository to mount.
    pub repo_path: PathBuf,
    /// Additional flags to pass to `docker create`.
    pub extra_flags: Vec<String>,
}

/// Isolation backend that runs tasks inside Docker containers.
#[derive(Debug)]
pub struct ContainerIsolation {
    config: ContainerConfig,
}

impl ContainerIsolation {
    /// Create a new container isolation backend.
    pub fn new(config: ContainerConfig) -> Self {
        Self { config }
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
        crate::worktree::WorktreeManager::branch_name(plan_name, task_name)
    }

    /// Create a git branch in the host repo for this task.
    async fn create_branch(&self, branch_name: &str) -> Result<()> {
        // Check if branch already exists.
        let check = Command::new("git")
            .args(["rev-parse", "--verify"])
            .arg(format!("refs/heads/{branch_name}"))
            .current_dir(&self.config.repo_path)
            .output()
            .await
            .context("failed to check branch existence")?;

        if check.status.success() {
            return Ok(()); // Branch already exists.
        }

        let output = Command::new("git")
            .args(["branch", branch_name])
            .current_dir(&self.config.repo_path)
            .output()
            .await
            .context("failed to create branch")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git branch {branch_name} failed: {stderr}");
        }

        Ok(())
    }
}

#[async_trait]
impl Isolation for ContainerIsolation {
    fn name(&self) -> &str {
        "container"
    }

    async fn create_workspace(&self, plan_name: &str, task_name: &str) -> Result<WorkspaceInfo> {
        let container_name = Self::container_name(plan_name, task_name);
        let branch_name = Self::branch_name(plan_name, task_name);

        // Create a branch in the host repo.
        self.create_branch(&branch_name).await?;

        // Build docker create command.
        let repo_path_str = self.config.repo_path.to_string_lossy();
        let volume_mount = format!("{repo_path_str}:/workspace");

        let mut args = vec![
            "create".to_string(),
            "--name".to_string(),
            container_name.clone(),
            "-v".to_string(),
            volume_mount,
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

        // Start the container.
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

        Ok(WorkspaceInfo {
            path: PathBuf::from("/workspace"),
            branch: Some(branch_name),
            container_id: Some(container_id),
        })
    }

    async fn remove_workspace(&self, info: &WorkspaceInfo) -> Result<()> {
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
        let config = ContainerConfig {
            image: "ubuntu:24.04".to_string(),
            repo_path: PathBuf::from("/tmp/repo"),
            extra_flags: vec![],
        };
        let iso = ContainerIsolation::new(config);
        assert_eq!(iso.name(), "container");
    }
}
