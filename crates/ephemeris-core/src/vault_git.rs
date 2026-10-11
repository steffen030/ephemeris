//! Optional git sync for Obsidian vaults that are git repositories.
//!
//! Uses the system `git` CLI (no libgit2). All helpers are safe to call from
//! background threads; failures are returned so callers can log and continue.

use std::path::Path;
use std::process::Command;

/// True when `vault_path/.git` exists (file or directory — covers worktrees).
pub fn is_git_repo(vault_path: &Path) -> bool {
    vault_path.join(".git").exists()
}

/// `git pull --rebase --autostash` in the vault. No-op-friendly on clean trees.
pub fn pull(vault_path: &Path) -> crate::Result<()> {
    run_git(vault_path, &["pull", "--rebase", "--autostash"])
}

/// Stage all changes, commit when there is something to commit, then push.
pub fn commit_and_push(vault_path: &Path, message: &str) -> crate::Result<()> {
    run_git(vault_path, &["add", "-A"])?;
    if !has_staged_or_unstaged_changes(vault_path)? {
        tracing::debug!("vault git: nothing to commit in {}", vault_path.display());
        return Ok(());
    }
    // `git commit` exits 1 when there is nothing staged after add — treat as ok.
    match run_git(vault_path, &["commit", "-m", message]) {
        Ok(()) => {}
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("nothing to commit") {
                return Ok(());
            }
            return Err(e);
        }
    }
    run_git(vault_path, &["push"])
}

fn has_staged_or_unstaged_changes(vault_path: &Path) -> crate::Result<bool> {
    let out = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(vault_path)
        .output()
        .map_err(crate::AppError::Io)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(crate::AppError::Storage(format!(
            "git status failed in {}: {stderr}",
            vault_path.display()
        )));
    }
    Ok(!out.stdout.is_empty())
}

fn run_git(vault_path: &Path, args: &[&str]) -> crate::Result<()> {
    let out = Command::new("git")
        .args(args)
        .current_dir(vault_path)
        .output()
        .map_err(crate::AppError::Io)?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    Err(crate::AppError::Storage(format!(
        "git {} failed in {}: {}{}",
        args.join(" "),
        vault_path.display(),
        stderr.trim(),
        if stdout.trim().is_empty() {
            String::new()
        } else {
            format!(" ({})", stdout.trim())
        }
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn detects_git_dir() {
        let tmp = TempDir::new().unwrap();
        assert!(!is_git_repo(tmp.path()));
        fs::create_dir(tmp.path().join(".git")).unwrap();
        assert!(is_git_repo(tmp.path()));
    }
}
