use std::path::{Path, PathBuf};

use {
    anyhow::{Context, Result, bail},
    tokio::process::Command,
    tracing::{debug, warn},
};

const WORKTREE_DIR: &str = ".moltis-worktrees";

/// Information about an active worktree.
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: String,
}

/// Manages git worktrees for session isolation.
pub struct WorktreeManager;

impl WorktreeManager {
    /// Create a new worktree for a session, branching from HEAD.
    pub async fn create(project_dir: &Path, session_id: &str) -> Result<PathBuf> {
        Self::ensure_git_repo(project_dir).await?;

        let branch = format!("moltis/{session_id}");
        let wt_dir = project_dir.join(WORKTREE_DIR).join(session_id);

        if wt_dir.exists() {
            return Ok(wt_dir);
        }

        let output = Command::new("git")
            .args(["worktree", "add", "-b", &branch])
            .arg(&wt_dir)
            .current_dir(project_dir)
            .output()
            .await
            .context("failed to run git worktree add")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git worktree add failed: {stderr}");
        }

        debug!(worktree = %wt_dir.display(), branch = %branch, "created worktree");
        Ok(wt_dir)
    }

    /// Create a worktree from an existing branch.
    pub async fn create_from_branch(
        project_dir: &Path,
        session_id: &str,
        branch: &str,
    ) -> Result<PathBuf> {
        Self::ensure_git_repo(project_dir).await?;

        let wt_dir = project_dir.join(WORKTREE_DIR).join(session_id);
        if wt_dir.exists() {
            return Ok(wt_dir);
        }

        let output = Command::new("git")
            .args(["worktree", "add"])
            .arg(&wt_dir)
            .arg(branch)
            .current_dir(project_dir)
            .output()
            .await
            .context("failed to run git worktree add")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git worktree add from branch failed: {stderr}");
        }

        debug!(worktree = %wt_dir.display(), branch = %branch, "created worktree from branch");
        Ok(wt_dir)
    }

    /// Remove a worktree and clean up the branch.
    /// If the branch was pushed to a remote, only the worktree is removed.
    /// If not pushed, the branch is also deleted.
    pub async fn cleanup(project_dir: &Path, session_id: &str) -> Result<()> {
        let wt_dir = project_dir.join(WORKTREE_DIR).join(session_id);
        let branch = format!("moltis/{session_id}");

        if wt_dir.exists() {
            let output = Command::new("git")
                .args(["worktree", "remove", "--force"])
                .arg(&wt_dir)
                .current_dir(project_dir)
                .output()
                .await
                .context("failed to run git worktree remove")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn!(stderr = %stderr, "git worktree remove failed, trying manual cleanup");
                // Force remove directory as fallback
                tokio::fs::remove_dir_all(&wt_dir).await.ok();
                // Prune stale worktree entries
                Command::new("git")
                    .args(["worktree", "prune"])
                    .current_dir(project_dir)
                    .output()
                    .await
                    .ok();
            }
        }

        // Check if branch was pushed
        let pushed = Self::is_branch_pushed(project_dir, &branch).await;
        if pushed {
            debug!(branch = %branch, "branch was pushed, keeping it");
        } else {
            // Delete local branch
            let output = Command::new("git")
                .args(["branch", "-D", &branch])
                .current_dir(project_dir)
                .output()
                .await;
            match output {
                Ok(o) if o.status.success() => {
                    debug!(branch = %branch, "deleted local branch");
                },
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    debug!(branch = %branch, stderr = %stderr, "branch delete skipped");
                },
                Err(e) => {
                    warn!(branch = %branch, error = %e, "failed to delete branch");
                },
            }
        }

        Ok(())
    }

    /// List active worktrees for a project.
    pub async fn list(project_dir: &Path) -> Result<Vec<WorktreeInfo>> {
        let output = Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(project_dir)
            .output()
            .await
            .context("failed to run git worktree list")?;

        if !output.status.success() {
            return Ok(Vec::new());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut worktrees = Vec::new();
        let mut current_path: Option<PathBuf> = None;
        let mut current_branch: Option<String> = None;

        for line in stdout.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                current_path = Some(PathBuf::from(path));
            } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
                current_branch = Some(branch.to_string());
            } else if line.is_empty() {
                if let (Some(path), Some(branch)) = (current_path.take(), current_branch.take()) {
                    // Only include moltis-managed worktrees
                    if branch.starts_with("moltis/") {
                        worktrees.push(WorktreeInfo { path, branch });
                    }
                }
                current_path = None;
                current_branch = None;
            }
        }
        // Handle last entry (no trailing blank line)
        if let (Some(path), Some(branch)) = (current_path, current_branch) {
            if branch.starts_with("moltis/") {
                worktrees.push(WorktreeInfo { path, branch });
            }
        }

        Ok(worktrees)
    }

    /// Run the project's setup command in a worktree directory.
    pub async fn run_setup(wt_dir: &Path, setup_command: &str) -> Result<()> {
        debug!(cmd = %setup_command, dir = %wt_dir.display(), "running setup command");
        let output = Command::new("sh")
            .args(["-c", setup_command])
            .current_dir(wt_dir)
            .output()
            .await
            .context("failed to run setup command")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!(cmd = %setup_command, stderr = %stderr, "setup command failed");
        }
        Ok(())
    }

    async fn ensure_git_repo(dir: &Path) -> Result<()> {
        if !dir.join(".git").exists() {
            bail!("{} is not a git repository", dir.display());
        }
        Ok(())
    }

    async fn is_branch_pushed(project_dir: &Path, branch: &str) -> bool {
        let output = Command::new("git")
            .args(["branch", "-r", "--list", &format!("origin/{branch}")])
            .current_dir(project_dir)
            .output()
            .await;
        match output {
            Ok(o) => !String::from_utf8_lossy(&o.stdout).trim().is_empty(),
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn init_test_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();
        dir
    }

    #[tokio::test]
    async fn test_create_and_list_worktree() {
        let dir = init_test_repo().await;
        let wt = WorktreeManager::create(dir.path(), "test-session")
            .await
            .unwrap();
        assert!(wt.exists());

        let list = WorktreeManager::list(dir.path()).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].branch, "moltis/test-session");
    }

    #[tokio::test]
    async fn test_create_idempotent() {
        let dir = init_test_repo().await;
        let wt1 = WorktreeManager::create(dir.path(), "sess").await.unwrap();
        let wt2 = WorktreeManager::create(dir.path(), "sess").await.unwrap();
        assert_eq!(wt1, wt2);
    }

    #[tokio::test]
    async fn test_cleanup_worktree() {
        let dir = init_test_repo().await;
        WorktreeManager::create(dir.path(), "sess").await.unwrap();
        WorktreeManager::cleanup(dir.path(), "sess").await.unwrap();
        let list = WorktreeManager::list(dir.path()).await.unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn test_not_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        let result = WorktreeManager::create(dir.path(), "x").await;
        assert!(result.is_err());
    }
}
