use chrono::Utc;
use colored::Colorize;
use dialoguer::{Confirm, Input, Select};
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

pub fn run(name: Option<&str>, base_name: Option<&str>, print: bool, open_claude: bool) -> Result<()> {
    let quiet = print;
    let config = Config::load()?;
    let interactive = name.is_none() || base_name.is_none();

    let name = match name {
        Some(n) => n.to_string(),
        None => {
            Input::<String>::new()
                .with_prompt("Workspace name")
                .validate_with(|input: &String| -> std::result::Result<(), String> {
                    if input.trim().is_empty() {
                        return Err("Name cannot be empty".into());
                    }
                    if WorkspaceConfig::exists(input).unwrap_or(false) {
                        return Err(format!("Workspace '{}' already exists", input));
                    }
                    Ok(())
                })
                .interact_text()
                .map_err(|e| Error::Git(e.to_string()))?
        }
    };

    let base_name = match base_name {
        Some(b) => b.to_string(),
        None => {
            let base_names: Vec<&String> = config.bases.keys().collect();
            if base_names.is_empty() {
                return Err(Error::Git("No bases configured. Add one with `cutter base add`.".into()));
            }
            let items: Vec<String> = base_names
                .iter()
                .map(|name| {
                    let base = &config.bases[*name];
                    let repos: Vec<&str> = base.repos.iter().map(|r| r.name.as_str()).collect();
                    format!("{} ({})", name, repos.join(", "))
                })
                .collect();
            let selection = Select::new()
                .with_prompt("Select a base")
                .items(&items)
                .interact()
                .map_err(|e| Error::Git(e.to_string()))?;
            base_names[selection].clone()
        }
    };

    let open_claude = if interactive && !open_claude && !print {
        Confirm::new()
            .with_prompt("Open with Claude after creation?")
            .default(false)
            .interact()
            .map_err(|e| Error::Git(e.to_string()))?
    } else {
        open_claude
    };

    let base = config
        .bases
        .get(&base_name)
        .ok_or_else(|| Error::BaseNotFound(base_name.to_string()))?;

    if WorkspaceConfig::exists(&name)? {
        return Err(Error::WorkspaceAlreadyExists(name.clone()));
    }

    let root = workspace_root_dir(&config);
    let workspace_dir = root.join(&name);

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

        match git::worktree_add(&source, &target, &name) {
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
