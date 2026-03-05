use chrono::Utc;
use colored::Colorize;
use std::path::PathBuf;

use crate::config::{Config, workspace_root_dir};
use crate::error::{Error, Result};
use crate::git;
use crate::workspace::{WorkspaceConfig, WorkspaceInfo, WorkspaceRepo};

macro_rules! info {
    ($quiet:expr, $($arg:tt)*) => {
        if $quiet {
            eprintln!($($arg)*);
        } else {
            println!($($arg)*);
        }
    };
}

pub fn run(name: &str, base_name: &str, print: bool, open_claude: bool) -> Result<()> {
    let quiet = print;
    let config = Config::load()?;

    let base = config
        .bases
        .get(base_name)
        .ok_or_else(|| Error::BaseNotFound(base_name.to_string()))?;

    if WorkspaceConfig::exists(name)? {
        return Err(Error::WorkspaceAlreadyExists(name.to_string()));
    }

    let root = workspace_root_dir(&config);
    let workspace_dir = root.join(name);

    // Validate all repos before creating anything
    for repo in &base.repos {
        let source = PathBuf::from(&repo.path);
        if !source.exists() {
            return Err(Error::PathNotFound(source));
        }
        if !git::is_git_repo(&source) {
            return Err(Error::NotAGitRepo(source));
        }
    }

    std::fs::create_dir_all(&workspace_dir)?;

    let mut workspace_repos = Vec::new();
    let mut created_worktrees = Vec::new();

    for repo in &base.repos {
        let source = PathBuf::from(&repo.path);
        let target = workspace_dir.join(&repo.name);

        match git::worktree_add(&source, &target, name) {
            Ok(()) => {
                created_worktrees.push((source.clone(), target.clone()));
                workspace_repos.push(WorkspaceRepo {
                    name: repo.name.clone(),
                    source: repo.path.clone(),
                    branch: name.to_string(),
                    worktree_path: target.to_string_lossy().to_string(),
                });
                info!(
                    quiet,
                    "  {} {} ({})",
                    "✓".green(),
                    repo.name.bold(),
                    target.display()
                );
            }
            Err(e) => {
                // Rollback created worktrees
                eprintln!("{} Failed to create worktree for '{}': {}", "✗".red(), repo.name, e);
                for (src, tgt) in &created_worktrees {
                    let _ = git::worktree_remove(src, tgt, true);
                }
                let _ = std::fs::remove_dir_all(&workspace_dir);
                return Err(e);
            }
        }
    }

    let ws_config = WorkspaceConfig {
        workspace: WorkspaceInfo {
            name: name.to_string(),
            base: base_name.to_string(),
            branch: name.to_string(),
            path: workspace_dir.to_string_lossy().to_string(),
            created_at: Utc::now(),
        },
        repos: workspace_repos,
    };
    ws_config.save()?;

    info!(
        quiet,
        "\n{} Workspace '{}' created at {}",
        "✓".green(),
        name.bold(),
        workspace_dir.display()
    );

    if print {
        println!("{}", workspace_dir.display());
    }
    if open_claude {
        let status = std::process::Command::new("claude")
            .current_dir(&workspace_dir)
            .status()?;
        if !status.success() {
            return Err(Error::Git("claude exited with non-zero status".into()));
        }
    }

    Ok(())
}
