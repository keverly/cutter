use colored::Colorize;
use std::path::PathBuf;

use crate::error::Result;
use crate::git;
use crate::workspace::WorkspaceConfig;

pub fn run(name: &str, keep_files: bool) -> Result<()> {
    let ws = WorkspaceConfig::load(name)?;

    for repo in &ws.repos {
        let source = PathBuf::from(&repo.source);
        let target = PathBuf::from(&repo.worktree_path);

        match git::worktree_remove(&source, &target, false) {
            Ok(()) => println!("  {} Removed worktree: {}", "✓".green(), repo.name),
            Err(e) => {
                eprintln!("  {} Failed to remove worktree '{}': {}", "✗".red(), repo.name, e);
                // Try force remove
                if git::worktree_remove(&source, &target, true).is_ok() {
                    println!("  {} Force removed worktree: {}", "✓".yellow(), repo.name);
                }
            }
        }

        // Attempt to delete the branch
        match git::delete_branch(&source, &repo.branch) {
            Ok(()) => println!("  {} Deleted branch '{}' from {}", "✓".green(), repo.branch, repo.name),
            Err(_) => {
                // Branch deletion is best-effort (may have unmerged changes)
            }
        }
    }

    if !keep_files {
        let workspace_path = PathBuf::from(&ws.workspace.path);
        if workspace_path.exists() {
            std::fs::remove_dir_all(&workspace_path)?;
            println!("  {} Removed directory: {}", "✓".green(), workspace_path.display());
        }
    }

    WorkspaceConfig::delete(name)?;
    println!("\n{} Workspace '{}' removed", "✓".green(), name.bold());
    Ok(())
}
