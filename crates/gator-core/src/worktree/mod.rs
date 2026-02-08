//! Git worktree management for agent isolation.
//!
//! Each agent task runs in its own git worktree, providing filesystem
//! isolation without the overhead of full repository clones. Worktrees
//! share the object store of the main repository but have independent
//! working directories and index files.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use thiserror::Error;

/// Errors that can occur during worktree operations.
#[derive(Debug, Error)]
pub enum WorktreeError {
    /// The main repository path does not exist or is not a git repository.
    #[error("not a git repository: {0}")]
    NotAGitRepo(PathBuf),

    /// A git command failed to execute.
    #[error("git command failed: {message}")]
    GitCommand {
        message: String,
        #[source]
        source: std::io::Error,
    },

    /// A git command exited with a non-zero status.
    #[error("git {command} failed (exit {code}): {stderr}")]
    GitExit {
        command: String,
        code: i32,
        stderr: String,
    },

    /// The worktree path already exists but is associated with a different
    /// branch than expected.
    #[error(
        "worktree path exists but has unexpected branch: expected {expected}, found {found}"
    )]
    BranchMismatch { expected: String, found: String },

    /// Failed to parse porcelain output from `git worktree list`.
    #[error("failed to parse worktree list output: {0}")]
    ParseError(String),
}

/// Result of a merge operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeResult {
    /// Merge completed successfully.
    Success,
    /// Merge had conflicts and was aborted.
    Conflict { details: String },
}

/// Information about a single git worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    /// Absolute path to the worktree directory.
    pub path: PathBuf,
    /// Branch checked out in this worktree, if any.
    pub branch: Option<String>,
    /// HEAD commit SHA.
    pub head_commit: String,
}

/// Manages git worktrees for agent isolation.
///
/// The manager operates relative to a main repository and places worktrees
/// in a configurable base directory (defaulting to a sibling directory of
/// the main repo named `<repo-name>-gator-worktrees`).
///
/// Git does not support concurrent worktree operations on the same
/// repository (it uses a lock file on the shared object store). This
/// manager serialises all mutating git operations through an internal
/// mutex so that concurrent lifecycle tasks do not race.
#[derive(Debug)]
pub struct WorktreeManager {
    /// Path to the main git repository.
    repo_path: PathBuf,
    /// Base directory under which worktrees are created.
    worktree_base: PathBuf,
    /// Serialises git operations to avoid lock-file contention.
    git_lock: Arc<Mutex<()>>,
}

impl Clone for WorktreeManager {
    fn clone(&self) -> Self {
        Self {
            repo_path: self.repo_path.clone(),
            worktree_base: self.worktree_base.clone(),
            git_lock: Arc::clone(&self.git_lock),
        }
    }
}

impl WorktreeManager {
    /// Create a new `WorktreeManager`.
    ///
    /// # Arguments
    ///
    /// * `repo_path` - Path to the main git repository.
    /// * `worktree_base` - Optional override for the worktree base directory.
    ///   If `None`, defaults to `../<repo-name>-gator-worktrees/` relative to
    ///   `repo_path`.
    ///
    /// # Errors
    ///
    /// Returns [`WorktreeError::NotAGitRepo`] if `repo_path` is not a git
    /// repository.
    pub fn new(
        repo_path: impl Into<PathBuf>,
        worktree_base: Option<PathBuf>,
    ) -> Result<Self, WorktreeError> {
        let repo_path = repo_path.into();

        // Verify this is a git repo by running `git rev-parse --git-dir`.
        let output = Command::new("git")
            .arg("rev-parse")
            .arg("--git-dir")
            .current_dir(&repo_path)
            .output()
            .map_err(|e| WorktreeError::GitCommand {
                message: "failed to run git rev-parse".into(),
                source: e,
            })?;

        if !output.status.success() {
            return Err(WorktreeError::NotAGitRepo(repo_path));
        }

        let worktree_base = worktree_base.unwrap_or_else(|| {
            let repo_name = repo_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("repo");
            let base_name = format!("{repo_name}-gator-worktrees");
            repo_path
                .parent()
                .map(|p| p.join(&base_name))
                .unwrap_or_else(|| PathBuf::from(base_name))
        });

        Ok(Self {
            repo_path,
            worktree_base,
            git_lock: Arc::new(Mutex::new(())),
        })
    }

    /// Return the base directory where worktrees are created.
    pub fn worktree_base(&self) -> &Path {
        &self.worktree_base
    }

    /// Return the main repository path.
    pub fn repo_path(&self) -> &Path {
        &self.repo_path
    }

    /// Build the conventional branch name for a plan/task pair.
    ///
    /// Format: `gator/<plan_name>/<task_name>`
    pub fn branch_name(plan_name: &str, task_name: &str) -> String {
        format!("gator/{plan_name}/{task_name}")
    }

    /// Create a new worktree with the given branch name.
    ///
    /// The worktree directory is placed under `worktree_base/<dir_name>`
    /// where `<dir_name>` is the branch name with `/` replaced by `--` for
    /// filesystem safety.
    ///
    /// This operation is **idempotent**: if a worktree already exists at the
    /// expected path with the expected branch, it is returned as-is.
    ///
    /// # Errors
    ///
    /// Returns an error if the git command fails. Any partial state (e.g. a
    /// directory that was created before the failure) is cleaned up on a
    /// best-effort basis.
    pub fn create_worktree(
        &self,
        branch_name: &str,
    ) -> Result<WorktreeInfo, WorktreeError> {
        let _lock = self.git_lock.lock().unwrap_or_else(|e| e.into_inner());

        let dir_name = branch_name.replace('/', "--");
        let worktree_path = self.worktree_base.join(&dir_name);

        // Check if this worktree already exists.
        if let Ok(existing) = self.find_worktree_by_path(&worktree_path) {
            // Verify the branch matches.
            if let Some(ref branch) = existing.branch {
                if branch == branch_name {
                    tracing::info!(
                        path = %worktree_path.display(),
                        branch = branch_name,
                        "worktree already exists, returning existing"
                    );
                    return Ok(existing);
                }
                return Err(WorktreeError::BranchMismatch {
                    expected: branch_name.to_string(),
                    found: branch.clone(),
                });
            }
            // Detached HEAD at the path -- treat as existing and return.
            tracing::info!(
                path = %worktree_path.display(),
                "worktree exists with detached HEAD, returning existing"
            );
            return Ok(existing);
        }

        // Ensure the worktree base directory exists.
        if !self.worktree_base.exists() {
            std::fs::create_dir_all(&self.worktree_base).map_err(|e| {
                WorktreeError::GitCommand {
                    message: format!(
                        "failed to create worktree base directory: {}",
                        self.worktree_base.display()
                    ),
                    source: e,
                }
            })?;
        }

        // Check if the branch already exists. If so, check it out in a new
        // worktree rather than creating a new branch.
        let branch_exists = self.branch_exists(branch_name)?;

        let output = if branch_exists {
            Command::new("git")
                .args(["worktree", "add"])
                .arg(&worktree_path)
                .arg(branch_name)
                .current_dir(&self.repo_path)
                .output()
                .map_err(|e| WorktreeError::GitCommand {
                    message: "failed to run git worktree add".into(),
                    source: e,
                })?
        } else {
            Command::new("git")
                .args(["worktree", "add", "-b"])
                .arg(branch_name)
                .arg(&worktree_path)
                .current_dir(&self.repo_path)
                .output()
                .map_err(|e| WorktreeError::GitCommand {
                    message: "failed to run git worktree add -b".into(),
                    source: e,
                })?
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            // Clean up partial state if a directory was created.
            self.cleanup_partial(&worktree_path);
            return Err(WorktreeError::GitExit {
                command: "worktree add".into(),
                code: output.status.code().unwrap_or(-1),
                stderr,
            });
        }

        // Read back the worktree info to confirm creation.
        self.find_worktree_by_path(&worktree_path)
    }

    /// Remove a worktree by its path.
    ///
    /// This removes the worktree directory and unregisters it from git.
    /// If the worktree does not exist, this is a no-op (idempotent).
    pub fn remove_worktree(&self, path: &Path) -> Result<(), WorktreeError> {
        let _lock = self.git_lock.lock().unwrap_or_else(|e| e.into_inner());

        // Check if the worktree exists in git's list first.
        if self.find_worktree_by_path(path).is_err() {
            // Worktree not registered. Clean up the directory if it exists.
            if path.exists() {
                tracing::warn!(
                    path = %path.display(),
                    "directory exists but not registered as worktree, removing"
                );
                let _ = std::fs::remove_dir_all(path);
            }
            return Ok(());
        }

        let output = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(path)
            .current_dir(&self.repo_path)
            .output()
            .map_err(|e| WorktreeError::GitCommand {
                message: "failed to run git worktree remove".into(),
                source: e,
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            // If the error says the worktree doesn't exist, treat as success.
            if stderr.contains("is not a working tree") {
                return Ok(());
            }
            return Err(WorktreeError::GitExit {
                command: "worktree remove".into(),
                code: output.status.code().unwrap_or(-1),
                stderr,
            });
        }

        Ok(())
    }

    /// List all worktrees associated with the main repository.
    pub fn list_worktrees(&self) -> Result<Vec<WorktreeInfo>, WorktreeError> {
        let output = Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(&self.repo_path)
            .output()
            .map_err(|e| WorktreeError::GitCommand {
                message: "failed to run git worktree list".into(),
                source: e,
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(WorktreeError::GitExit {
                command: "worktree list".into(),
                code: output.status.code().unwrap_or(-1),
                stderr,
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_porcelain_output(&stdout)
    }

    /// Prune stale worktree entries.
    ///
    /// Runs `git worktree prune` to clean up references to worktrees
    /// whose directories have been removed externally.
    pub fn cleanup_stale(&self) -> Result<(), WorktreeError> {
        let output = Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(&self.repo_path)
            .output()
            .map_err(|e| WorktreeError::GitCommand {
                message: "failed to run git worktree prune".into(),
                source: e,
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(WorktreeError::GitExit {
                command: "worktree prune".into(),
                code: output.status.code().unwrap_or(-1),
                stderr,
            });
        }

        Ok(())
    }

    /// Merge a branch into the current branch of the main repo using `--no-ff`.
    ///
    /// Returns `Ok(true)` on success, `Ok(false)` if there were merge conflicts
    /// (the merge is aborted automatically). Returns `Err` on other git failures.
    pub fn merge_branch(&self, branch_name: &str) -> Result<MergeResult, WorktreeError> {
        let _lock = self.git_lock.lock().unwrap_or_else(|e| e.into_inner());

        let output = Command::new("git")
            .args(["merge", "--no-ff", branch_name])
            .current_dir(&self.repo_path)
            .output()
            .map_err(|e| WorktreeError::GitCommand {
                message: "failed to run git merge".into(),
                source: e,
            })?;

        if output.status.success() {
            return Ok(MergeResult::Success);
        }

        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();

        // Check for merge conflict indicators.
        if stderr.contains("CONFLICT") || stdout.contains("CONFLICT") || stderr.contains("Automatic merge failed") {
            // Abort the conflicted merge.
            let _ = Command::new("git")
                .args(["merge", "--abort"])
                .current_dir(&self.repo_path)
                .output();

            return Ok(MergeResult::Conflict {
                details: format!("{stdout}\n{stderr}").trim().to_string(),
            });
        }

        Err(WorktreeError::GitExit {
            command: "merge".into(),
            code: output.status.code().unwrap_or(-1),
            stderr,
        })
    }

    /// Delete a local branch.
    ///
    /// Uses `-D` (force delete) since the branch may not be fully merged
    /// into the current branch (it was merged via `--no-ff`).
    /// Returns `Ok(())` even if the branch doesn't exist (idempotent).
    pub fn delete_branch(&self, branch_name: &str) -> Result<(), WorktreeError> {
        let _lock = self.git_lock.lock().unwrap_or_else(|e| e.into_inner());

        let output = Command::new("git")
            .args(["branch", "-D", branch_name])
            .current_dir(&self.repo_path)
            .output()
            .map_err(|e| WorktreeError::GitCommand {
                message: "failed to run git branch -D".into(),
                source: e,
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            // Branch not found is not an error for idempotency.
            if stderr.contains("not found") {
                return Ok(());
            }
            return Err(WorktreeError::GitExit {
                command: "branch -D".into(),
                code: output.status.code().unwrap_or(-1),
                stderr,
            });
        }

        Ok(())
    }

    /// Checkout a branch in the main repository.
    pub fn checkout(&self, branch_name: &str) -> Result<(), WorktreeError> {
        let _lock = self.git_lock.lock().unwrap_or_else(|e| e.into_inner());

        let output = Command::new("git")
            .args(["checkout", branch_name])
            .current_dir(&self.repo_path)
            .output()
            .map_err(|e| WorktreeError::GitCommand {
                message: "failed to run git checkout".into(),
                source: e,
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(WorktreeError::GitExit {
                command: "checkout".into(),
                code: output.status.code().unwrap_or(-1),
                stderr,
            });
        }

        Ok(())
    }

    /// Check whether a branch exists in the repository.
    pub fn branch_exists(&self, branch_name: &str) -> Result<bool, WorktreeError> {
        let output = Command::new("git")
            .args(["rev-parse", "--verify"])
            .arg(format!("refs/heads/{branch_name}"))
            .current_dir(&self.repo_path)
            .output()
            .map_err(|e| WorktreeError::GitCommand {
                message: "failed to run git rev-parse --verify".into(),
                source: e,
            })?;

        Ok(output.status.success())
    }

    /// Find a worktree by its path in the worktree list.
    fn find_worktree_by_path(
        &self,
        path: &Path,
    ) -> Result<WorktreeInfo, WorktreeError> {
        let worktrees = self.list_worktrees()?;
        // Canonicalize for comparison where possible.
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

        for wt in worktrees {
            let wt_canonical = wt
                .path
                .canonicalize()
                .unwrap_or_else(|_| wt.path.clone());
            if wt_canonical == canonical {
                return Ok(wt);
            }
        }

        Err(WorktreeError::ParseError(format!(
            "worktree not found at path: {}",
            path.display()
        )))
    }

    /// Best-effort cleanup of a partially created worktree directory.
    fn cleanup_partial(&self, path: &Path) {
        if path.exists() {
            tracing::warn!(
                path = %path.display(),
                "cleaning up partial worktree directory"
            );
            let _ = std::fs::remove_dir_all(path);
        }
        // Also try to prune stale entries.
        let _ = self.cleanup_stale();
    }
}

/// Parse the porcelain output of `git worktree list --porcelain`.
///
/// The format consists of blocks separated by blank lines. Each block has:
///
/// ```text
/// worktree <path>
/// HEAD <sha>
/// branch refs/heads/<name>
/// ```
///
/// The main worktree may show `bare` instead of `branch`, and detached
/// worktrees show `detached` instead of `branch`.
fn parse_porcelain_output(output: &str) -> Result<Vec<WorktreeInfo>, WorktreeError> {
    let mut worktrees = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_head: Option<String> = None;
    let mut current_branch: Option<String> = None;

    for line in output.lines() {
        if line.is_empty() {
            // End of a block -- commit the current entry if we have one.
            if let (Some(path), Some(head)) = (current_path.take(), current_head.take())
            {
                worktrees.push(WorktreeInfo {
                    path,
                    branch: current_branch.take(),
                    head_commit: head,
                });
            } else {
                current_path = None;
                current_head = None;
                current_branch = None;
            }
            continue;
        }

        if let Some(rest) = line.strip_prefix("worktree ") {
            current_path = Some(PathBuf::from(rest));
        } else if let Some(rest) = line.strip_prefix("HEAD ") {
            current_head = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("branch ") {
            // Strip the refs/heads/ prefix to get the short branch name.
            let branch = rest
                .strip_prefix("refs/heads/")
                .unwrap_or(rest)
                .to_string();
            current_branch = Some(branch);
        }
        // Ignore `bare`, `detached`, `prunable`, etc.
    }

    // Handle the last block (porcelain output may not end with a blank line).
    if let (Some(path), Some(head)) = (current_path, current_head) {
        worktrees.push(WorktreeInfo {
            path,
            branch: current_branch,
            head_commit: head,
        });
    }

    Ok(worktrees)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    /// Create a temporary git repository with an initial commit.
    /// Returns the TempDir (must be held alive) and the repo path.
    fn create_temp_repo() -> (TempDir, PathBuf) {
        let dir = TempDir::new().expect("failed to create temp dir");
        let repo_path = dir.path().to_path_buf();

        // Initialize a git repository.
        let status = Command::new("git")
            .args(["init"])
            .current_dir(&repo_path)
            .output()
            .expect("failed to run git init");
        assert!(status.status.success(), "git init failed");

        // Configure user for commits.
        let _ = Command::new("git")
            .args(["config", "user.email", "test@gator.dev"])
            .current_dir(&repo_path)
            .output()
            .expect("failed to configure email");

        let _ = Command::new("git")
            .args(["config", "user.name", "Gator Test"])
            .current_dir(&repo_path)
            .output()
            .expect("failed to configure name");

        // Create an initial commit so HEAD exists.
        let readme = repo_path.join("README.md");
        std::fs::write(&readme, "# Test repo\n").expect("failed to write README");

        let _ = Command::new("git")
            .args(["add", "."])
            .current_dir(&repo_path)
            .output()
            .expect("failed to run git add");

        let status = Command::new("git")
            .args(["commit", "-m", "Initial commit"])
            .current_dir(&repo_path)
            .output()
            .expect("failed to run git commit");
        assert!(status.status.success(), "git commit failed");

        (dir, repo_path)
    }

    #[test]
    fn test_new_with_valid_repo() {
        let (_dir, repo_path) = create_temp_repo();
        let mgr = WorktreeManager::new(&repo_path, None);
        assert!(mgr.is_ok());
        let mgr = mgr.unwrap();
        assert_eq!(mgr.repo_path(), repo_path);
    }

    #[test]
    fn test_new_with_invalid_repo() {
        let dir = TempDir::new().expect("failed to create temp dir");
        let result = WorktreeManager::new(dir.path(), None);
        assert!(result.is_err());
        assert!(matches!(result, Err(WorktreeError::NotAGitRepo(_))));
    }

    #[test]
    fn test_default_worktree_base() {
        let (_dir, repo_path) = create_temp_repo();
        let mgr = WorktreeManager::new(&repo_path, None).unwrap();

        let repo_name = repo_path.file_name().unwrap().to_str().unwrap();
        let expected_base = repo_path
            .parent()
            .unwrap()
            .join(format!("{repo_name}-gator-worktrees"));
        assert_eq!(mgr.worktree_base(), expected_base);
    }

    #[test]
    fn test_custom_worktree_base() {
        let (_dir, repo_path) = create_temp_repo();
        let custom_base = repo_path.join("my-worktrees");
        let mgr =
            WorktreeManager::new(&repo_path, Some(custom_base.clone())).unwrap();
        assert_eq!(mgr.worktree_base(), custom_base);
    }

    #[test]
    fn test_branch_name() {
        assert_eq!(
            WorktreeManager::branch_name("add-auth", "implement-jwt"),
            "gator/add-auth/implement-jwt"
        );
    }

    #[test]
    fn test_create_and_list_worktree() {
        let (_dir, repo_path) = create_temp_repo();
        let worktree_base = TempDir::new().expect("failed to create worktree base");
        let mgr = WorktreeManager::new(
            &repo_path,
            Some(worktree_base.path().to_path_buf()),
        )
        .unwrap();

        let branch = WorktreeManager::branch_name("test-plan", "test-task");
        let info = mgr.create_worktree(&branch).expect("create_worktree failed");

        assert!(info.path.exists(), "worktree directory should exist");
        assert_eq!(info.branch.as_deref(), Some(branch.as_str()));
        assert!(!info.head_commit.is_empty());

        // Verify the worktree appears in the list.
        let worktrees = mgr.list_worktrees().expect("list_worktrees failed");
        // Should have at least 2: the main worktree + our new one.
        assert!(worktrees.len() >= 2);

        let found = worktrees
            .iter()
            .any(|wt| wt.branch.as_deref() == Some(branch.as_str()));
        assert!(found, "created worktree should appear in list");
    }

    #[test]
    fn test_create_worktree_idempotent() {
        let (_dir, repo_path) = create_temp_repo();
        let worktree_base = TempDir::new().expect("failed to create worktree base");
        let mgr = WorktreeManager::new(
            &repo_path,
            Some(worktree_base.path().to_path_buf()),
        )
        .unwrap();

        let branch = WorktreeManager::branch_name("plan", "idempotent-task");

        let info1 = mgr.create_worktree(&branch).expect("first create failed");
        let info2 = mgr.create_worktree(&branch).expect("second create failed");

        // Both should return the same worktree.
        assert_eq!(info1.path, info2.path);
        assert_eq!(info1.branch, info2.branch);
    }

    #[test]
    fn test_remove_worktree() {
        let (_dir, repo_path) = create_temp_repo();
        let worktree_base = TempDir::new().expect("failed to create worktree base");
        let mgr = WorktreeManager::new(
            &repo_path,
            Some(worktree_base.path().to_path_buf()),
        )
        .unwrap();

        let branch = WorktreeManager::branch_name("plan", "remove-task");
        let info = mgr.create_worktree(&branch).expect("create failed");

        assert!(info.path.exists());

        mgr.remove_worktree(&info.path).expect("remove failed");

        assert!(!info.path.exists(), "worktree directory should be removed");

        // Verify it no longer appears in the list.
        let worktrees = mgr.list_worktrees().expect("list failed");
        let found = worktrees.iter().any(|wt| wt.path == info.path);
        assert!(!found, "removed worktree should not appear in list");
    }

    #[test]
    fn test_remove_worktree_idempotent() {
        let (_dir, repo_path) = create_temp_repo();
        let worktree_base = TempDir::new().expect("failed to create worktree base");
        let mgr = WorktreeManager::new(
            &repo_path,
            Some(worktree_base.path().to_path_buf()),
        )
        .unwrap();

        let branch = WorktreeManager::branch_name("plan", "remove-idem");
        let info = mgr.create_worktree(&branch).expect("create failed");

        mgr.remove_worktree(&info.path)
            .expect("first remove failed");
        // Second remove should be a no-op.
        mgr.remove_worktree(&info.path)
            .expect("second remove should not fail");
    }

    #[test]
    fn test_list_worktrees_includes_main() {
        let (_dir, repo_path) = create_temp_repo();
        let mgr = WorktreeManager::new(&repo_path, None).unwrap();

        let worktrees = mgr.list_worktrees().expect("list failed");
        assert!(
            !worktrees.is_empty(),
            "should include at least the main worktree"
        );

        // The main worktree should be at the repo path.
        let repo_canonical = repo_path.canonicalize().unwrap();
        let found = worktrees.iter().any(|wt| {
            wt.path
                .canonicalize()
                .unwrap_or_else(|_| wt.path.clone())
                == repo_canonical
        });
        assert!(found, "main worktree should be in the list");
    }

    #[test]
    fn test_cleanup_stale() {
        let (_dir, repo_path) = create_temp_repo();
        let worktree_base = TempDir::new().expect("failed to create worktree base");
        let mgr = WorktreeManager::new(
            &repo_path,
            Some(worktree_base.path().to_path_buf()),
        )
        .unwrap();

        let branch = WorktreeManager::branch_name("plan", "stale-task");
        let info = mgr.create_worktree(&branch).expect("create failed");

        // Manually remove the worktree directory to simulate stale state.
        std::fs::remove_dir_all(&info.path).expect("manual remove failed");

        // Prune should clean up the stale reference.
        mgr.cleanup_stale().expect("cleanup_stale failed");

        // After pruning, the worktree should no longer appear in the list.
        let worktrees = mgr.list_worktrees().expect("list failed");
        let found = worktrees
            .iter()
            .any(|wt| wt.branch.as_deref() == Some(branch.as_str()));
        assert!(
            !found,
            "stale worktree should be removed after cleanup_stale"
        );
    }

    #[test]
    fn test_create_multiple_worktrees() {
        let (_dir, repo_path) = create_temp_repo();
        let worktree_base = TempDir::new().expect("failed to create worktree base");
        let mgr = WorktreeManager::new(
            &repo_path,
            Some(worktree_base.path().to_path_buf()),
        )
        .unwrap();

        let branch1 = WorktreeManager::branch_name("plan", "task-1");
        let branch2 = WorktreeManager::branch_name("plan", "task-2");
        let branch3 = WorktreeManager::branch_name("plan", "task-3");

        let info1 = mgr.create_worktree(&branch1).expect("create 1 failed");
        let info2 = mgr.create_worktree(&branch2).expect("create 2 failed");
        let info3 = mgr.create_worktree(&branch3).expect("create 3 failed");

        // All should be distinct paths.
        assert_ne!(info1.path, info2.path);
        assert_ne!(info2.path, info3.path);
        assert_ne!(info1.path, info3.path);

        // All should appear in the list.
        let worktrees = mgr.list_worktrees().expect("list failed");
        // main + 3 worktrees = at least 4.
        assert!(worktrees.len() >= 4);
    }

    #[test]
    fn test_worktree_has_correct_content() {
        let (_dir, repo_path) = create_temp_repo();
        let worktree_base = TempDir::new().expect("failed to create worktree base");
        let mgr = WorktreeManager::new(
            &repo_path,
            Some(worktree_base.path().to_path_buf()),
        )
        .unwrap();

        let branch = WorktreeManager::branch_name("plan", "content-check");
        let info = mgr.create_worktree(&branch).expect("create failed");

        // The worktree should contain the same files as the main repo.
        let readme = info.path.join("README.md");
        assert!(readme.exists(), "README.md should exist in worktree");

        let content =
            std::fs::read_to_string(&readme).expect("failed to read README");
        assert_eq!(content, "# Test repo\n");
    }

    #[test]
    fn test_worktree_isolation() {
        let (_dir, repo_path) = create_temp_repo();
        let worktree_base = TempDir::new().expect("failed to create worktree base");
        let mgr = WorktreeManager::new(
            &repo_path,
            Some(worktree_base.path().to_path_buf()),
        )
        .unwrap();

        let branch = WorktreeManager::branch_name("plan", "isolation-task");
        let info = mgr.create_worktree(&branch).expect("create failed");

        // Write a new file in the worktree.
        let new_file = info.path.join("agent-work.txt");
        std::fs::write(&new_file, "agent output\n").expect("write failed");

        // The file should NOT exist in the main repo.
        let main_file = repo_path.join("agent-work.txt");
        assert!(
            !main_file.exists(),
            "file created in worktree should not appear in main repo"
        );
    }

    #[test]
    fn test_parse_porcelain_output() {
        let input = "\
worktree /home/user/project
HEAD abc123def456
branch refs/heads/main

worktree /home/user/worktrees/feature
HEAD 789abc012def
branch refs/heads/gator/plan/task

worktree /home/user/worktrees/detached
HEAD 111222333444
detached

";
        let result = parse_porcelain_output(input).unwrap();
        assert_eq!(result.len(), 3);

        assert_eq!(result[0].path, PathBuf::from("/home/user/project"));
        assert_eq!(result[0].head_commit, "abc123def456");
        assert_eq!(result[0].branch.as_deref(), Some("main"));

        assert_eq!(
            result[1].path,
            PathBuf::from("/home/user/worktrees/feature")
        );
        assert_eq!(result[1].head_commit, "789abc012def");
        assert_eq!(
            result[1].branch.as_deref(),
            Some("gator/plan/task")
        );

        assert_eq!(
            result[2].path,
            PathBuf::from("/home/user/worktrees/detached")
        );
        assert_eq!(result[2].head_commit, "111222333444");
        assert_eq!(result[2].branch, None);
    }

    #[test]
    fn test_parse_porcelain_output_no_trailing_newline() {
        let input = "\
worktree /home/user/project
HEAD abc123
branch refs/heads/main";
        let result = parse_porcelain_output(input).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].branch.as_deref(), Some("main"));
    }

    #[test]
    fn test_parse_porcelain_output_empty() {
        let result = parse_porcelain_output("").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_cleanup_on_failure_removes_directory() {
        let (_dir, repo_path) = create_temp_repo();
        let worktree_base = TempDir::new().expect("failed to create worktree base");
        let mgr = WorktreeManager::new(
            &repo_path,
            Some(worktree_base.path().to_path_buf()),
        )
        .unwrap();

        // Find the current branch of the main worktree. Attempting to
        // create a worktree for the same branch will fail because git
        // refuses to check out a branch that is already checked out.
        let output = Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(&repo_path)
            .output()
            .expect("failed to get current branch");
        let main_branch =
            String::from_utf8_lossy(&output.stdout).trim().to_string();

        if !main_branch.is_empty() {
            // Try to create a worktree for the already checked-out branch.
            let result = mgr.create_worktree(&main_branch);
            assert!(
                result.is_err(),
                "creating worktree for already-checked-out branch should fail"
            );

            // Verify no partial directory remains.
            let dir_name = main_branch.replace('/', "--");
            let partial_path = worktree_base.path().join(dir_name);
            assert!(
                !partial_path.exists(),
                "partial worktree directory should be cleaned up"
            );
        }
    }

    #[test]
    fn test_merge_branch_success() {
        let (_dir, repo_path) = create_temp_repo();
        let worktree_base = TempDir::new().expect("failed to create worktree base");
        let mgr = WorktreeManager::new(
            &repo_path,
            Some(worktree_base.path().to_path_buf()),
        )
        .unwrap();

        // Create a worktree, make a commit in it, then merge back.
        let branch = WorktreeManager::branch_name("plan", "merge-task");
        let info = mgr.create_worktree(&branch).expect("create failed");

        // Write a new file in the worktree and commit it.
        let new_file = info.path.join("feature.txt");
        std::fs::write(&new_file, "new feature\n").expect("write failed");

        let run = |args: &[&str], dir: &std::path::Path| {
            let output = Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap_or_else(|e| panic!("git {} failed: {e}", args.join(" ")));
            assert!(
                output.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
        };

        run(&["add", "feature.txt"], &info.path);
        run(&["commit", "-m", "Add feature"], &info.path);

        // Remove the worktree first so the branch can be checked out.
        mgr.remove_worktree(&info.path).expect("remove failed");

        // Merge the branch back into main.
        let result = mgr.merge_branch(&branch).expect("merge failed");
        assert_eq!(result, MergeResult::Success);

        // Verify the file exists in the main repo.
        let merged_file = repo_path.join("feature.txt");
        assert!(merged_file.exists(), "merged file should exist in main repo");
    }

    #[test]
    fn test_delete_branch() {
        let (_dir, repo_path) = create_temp_repo();
        let worktree_base = TempDir::new().expect("failed to create worktree base");
        let mgr = WorktreeManager::new(
            &repo_path,
            Some(worktree_base.path().to_path_buf()),
        )
        .unwrap();

        let branch = WorktreeManager::branch_name("plan", "delete-task");
        let info = mgr.create_worktree(&branch).expect("create failed");
        mgr.remove_worktree(&info.path).expect("remove failed");

        // Branch should exist before deletion.
        assert!(mgr.branch_exists(&branch).unwrap());

        mgr.delete_branch(&branch).expect("delete failed");

        // Branch should no longer exist.
        assert!(!mgr.branch_exists(&branch).unwrap());
    }

    #[test]
    fn test_delete_branch_idempotent() {
        let (_dir, repo_path) = create_temp_repo();
        let mgr = WorktreeManager::new(&repo_path, None).unwrap();

        // Deleting a non-existent branch should not error.
        mgr.delete_branch("gator/nonexistent/branch")
            .expect("deleting nonexistent branch should not fail");
    }
}
