//! Optional git sync for Obsidian vaults that are git repositories.
//!
//! Uses the system `git` CLI (no libgit2). All helpers are safe to call from
//! background threads; failures are returned so callers can log and continue.
//!
//! Conflict policy for Ephemeris export content (`export_subdir`, typically
//! `Ephemeris/`): **local Ephemeris wins**. Remote edits to that tree lose on
//! pull/rebase so device-authored notes/PDFs stay authoritative.

use std::path::{Path, PathBuf};
use std::process::Command;

/// True when `vault_path/.git` exists (file or directory — covers worktrees).
pub fn is_git_repo(vault_path: &Path) -> bool {
    vault_path.join(".git").exists()
}

/// `git pull --rebase --autostash` in the vault.
///
/// When `export_subdir` is set, conflicts under that tree are resolved in
/// favour of the local (Ephemeris) side and the rebase/merge is continued.
pub fn pull(vault_path: &Path, export_subdir: Option<&Path>) -> crate::Result<()> {
    match run_git(vault_path, &["pull", "--rebase", "--autostash"]) {
        Ok(()) => Ok(()),
        Err(e) if export_subdir.is_some() && looks_like_conflict(&e) => {
            let sub = export_subdir.unwrap();
            resolve_export_conflicts_ephemeris_wins(vault_path, sub)?;
            continue_after_conflict(vault_path, sub)?;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Stage all changes, commit when there is something to commit, then push.
///
/// On non-fast-forward push rejection, pulls (with Ephemeris-wins resolution
/// under `export_subdir`) and retries the push once.
pub fn commit_and_push(
    vault_path: &Path,
    message: &str,
    export_subdir: &Path,
) -> crate::Result<()> {
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
    match run_git(vault_path, &["push"]) {
        Ok(()) => Ok(()),
        Err(e) if looks_like_non_fast_forward(&e) => {
            tracing::info!(
                "vault git: push rejected, pulling with Ephemeris-wins then retrying ({})",
                vault_path.display()
            );
            pull(vault_path, Some(export_subdir))?;
            run_git(vault_path, &["push"])
        }
        Err(e) => Err(e),
    }
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
    run_git_env(vault_path, args, &[])
}

fn run_git_env(vault_path: &Path, args: &[&str], env: &[(&str, &str)]) -> crate::Result<()> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(vault_path);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().map_err(crate::AppError::Io)?;
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

fn looks_like_conflict(err: &crate::AppError) -> bool {
    let msg = err.to_string().to_lowercase();
    msg.contains("conflict")
        || msg.contains("needs merge")
        || msg.contains("could not apply")
        || msg.contains("unmerged paths")
        || msg.contains("fix conflicts")
}

fn looks_like_non_fast_forward(err: &crate::AppError) -> bool {
    let msg = err.to_string().to_lowercase();
    msg.contains("non-fast-forward")
        || msg.contains("fetch first")
        || msg.contains("updates were rejected")
        || msg.contains("failed to push")
}

fn is_rebase_in_progress(vault_path: &Path) -> bool {
    let git = vault_path.join(".git");
    // Plain repo: `.git/rebase-merge`. Worktree: `.git` may be a file — also
    // check common alternate locations via `git rev-parse`.
    if git.join("rebase-merge").exists() || git.join("rebase-apply").exists() {
        return true;
    }
    let out = Command::new("git")
        .args(["rev-parse", "--git-path", "rebase-merge"])
        .current_dir(vault_path)
        .output();
    if let Ok(out) = out {
        if out.status.success() {
            let rel = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !rel.is_empty() {
                let path = if Path::new(&rel).is_absolute() {
                    PathBuf::from(&rel)
                } else {
                    vault_path.join(&rel)
                };
                if path.exists() {
                    return true;
                }
            }
        }
    }
    false
}

fn unmerged_paths(vault_path: &Path) -> crate::Result<Vec<String>> {
    let out = Command::new("git")
        .args(["diff", "--name-only", "--diff-filter=U"])
        .current_dir(vault_path)
        .output()
        .map_err(crate::AppError::Io)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(crate::AppError::Storage(format!(
            "git diff --diff-filter=U failed in {}: {stderr}",
            vault_path.display()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

fn path_under_export(path: &str, export_subdir: &Path) -> bool {
    let export = export_subdir.to_string_lossy().replace('\\', "/");
    let export = export.trim_matches('/');
    if export.is_empty() {
        return false;
    }
    let p = path.replace('\\', "/");
    p == export || p.starts_with(&format!("{export}/"))
}

/// Resolve unmerged paths under `export_subdir` keeping the Ephemeris side.
///
/// During a **rebase**, `--theirs` is the commit being replayed (local).
/// During a **merge**, `--ours` is the current branch (local).
fn resolve_export_conflicts_ephemeris_wins(
    vault_path: &Path,
    export_subdir: &Path,
) -> crate::Result<()> {
    let side = if is_rebase_in_progress(vault_path) {
        "--theirs"
    } else {
        "--ours"
    };
    let unmerged = unmerged_paths(vault_path)?;
    let mut resolved = 0usize;
    let mut remaining = 0usize;
    for path in &unmerged {
        if path_under_export(path, export_subdir) {
            run_git(vault_path, &["checkout", side, "--", path])?;
            run_git(vault_path, &["add", "--", path])?;
            resolved += 1;
            tracing::info!(
                "vault git: Ephemeris wins conflict on {path} ({side} in {})",
                vault_path.display()
            );
        } else {
            remaining += 1;
        }
    }
    if remaining > 0 {
        return Err(crate::AppError::Storage(format!(
            "vault git: {remaining} conflict(s) outside export tree {} remain in {}",
            export_subdir.display(),
            vault_path.display()
        )));
    }
    if resolved == 0 && !unmerged.is_empty() {
        return Err(crate::AppError::Storage(format!(
            "vault git: conflicts present but none under {} in {}",
            export_subdir.display(),
            vault_path.display()
        )));
    }
    Ok(())
}

fn continue_after_conflict(vault_path: &Path, export_subdir: &Path) -> crate::Result<()> {
    // Rebase may conflict on successive commits — resolve in a loop.
    for _ in 0..32 {
        if is_rebase_in_progress(vault_path) {
            match run_git_env(
                vault_path,
                &["rebase", "--continue"],
                &[("GIT_EDITOR", "true"), ("EDITOR", "true")],
            ) {
                Ok(()) => return Ok(()),
                Err(e) if looks_like_conflict(&e) => {
                    resolve_export_conflicts_ephemeris_wins(vault_path, export_subdir)?;
                    continue;
                }
                Err(e) => {
                    let msg = e.to_string().to_lowercase();
                    if msg.contains("no rebase in progress") {
                        return Ok(());
                    }
                    return Err(e);
                }
            }
        }
        // Merge conflict: create the merge commit if one is pending.
        let unmerged = unmerged_paths(vault_path)?;
        if unmerged.is_empty() {
            // Try completing a merge commit when index is ready.
            match run_git_env(
                vault_path,
                &["commit", "--no-edit"],
                &[("GIT_EDITOR", "true"), ("EDITOR", "true")],
            ) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let msg = e.to_string().to_lowercase();
                    if msg.contains("nothing to commit")
                        || msg.contains("no changes")
                        || msg.contains("not currently on a merge")
                    {
                        return Ok(());
                    }
                    // Still mid-pull with no unmerged paths — treat as done.
                    return Ok(());
                }
            }
        }
        resolve_export_conflicts_ephemeris_wins(vault_path, export_subdir)?;
    }
    Err(crate::AppError::Storage(format!(
        "vault git: gave up continuing after conflicts in {}",
        vault_path.display()
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

    #[test]
    fn path_under_export_matches_prefix() {
        let sub = Path::new("Ephemeris");
        assert!(path_under_export("Ephemeris/Notes/a.md", sub));
        assert!(path_under_export("Ephemeris", sub));
        assert!(!path_under_export("Tasks.md", sub));
        assert!(!path_under_export("EphemerisBackup/x.md", sub));
    }

    #[test]
    fn conflict_and_nff_detectors() {
        let conflict = crate::AppError::Storage("CONFLICT (content): merge failed".into());
        assert!(looks_like_conflict(&conflict));
        let nff = crate::AppError::Storage(
            "Updates were rejected because the remote contains work you do not have".into(),
        );
        assert!(looks_like_non_fast_forward(&nff));
    }
}
