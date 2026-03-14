use chrono::Utc;
use colored::Colorize;
use dialoguer::{Input, Select};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::cli::ClaudeMode;
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

pub fn run(name: Option<&str>, base_name: Option<&str>, print: bool, claude_mode: ClaudeMode) -> Result<()> {
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

    let claude_mode = if interactive && claude_mode == ClaudeMode::None && !print {
        let items = &["No", "Claude", "Claude (--dangerously-skip-permissions)"];
        let selection = Select::new()
            .with_prompt("Open with Claude after creation?")
            .items(items)
            .default(0)
            .interact()
            .map_err(|e| Error::Git(e.to_string()))?;
        match selection {
            1 => ClaudeMode::Normal,
            2 => ClaudeMode::DangerouslySkipPermissions,
            _ => ClaudeMode::None,
        }
    } else {
        claude_mode
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

    let base_branch_from = base
        .branch_from
        .as_deref()
        .unwrap_or(&config.settings.default_branch_from);

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

    // Fetch latest from remotes
    for repo in &base.repos {
        let source = PathBuf::from(&repo.path);
        info!(quiet, "  {} Fetching {}", "⟳".cyan(), repo.name.bold());
        git::fetch(&source)?;
    }

    std::fs::create_dir_all(&workspace_dir)?;

    let mut workspace_repos = Vec::new();
    let mut created_worktrees = Vec::new();

    for repo in &base.repos {
        let source = PathBuf::from(&repo.path);
        let target = workspace_dir.join(&repo.name);
        let branch_from = repo.branch_from.as_deref().unwrap_or(base_branch_from);

        match git::worktree_add(&source, &target, &name, Some(branch_from)) {
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

    // Copy configured files from source repos into worktrees (e.g. .env)
    if !base.copy_files.is_empty() {
        copy_base_files(&base.copy_files, &base.repos, &workspace_dir, quiet)?;
    }

    // Merge .claude directories from each repo into workspace root
    merge_claude_dirs(&workspace_dir, &created_worktrees, quiet)?;

    // Overlay base-level .claude directory on top of merged result
    overlay_base_claude_dir(&workspace_dir, &base_name, quiet)?;

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
    match claude_mode {
        ClaudeMode::Normal => {
            let status = std::process::Command::new("claude")
                .current_dir(&workspace_dir)
                .status()?;
            if !status.success() {
                return Err(Error::Git("claude exited with non-zero status".into()));
            }
        }
        ClaudeMode::DangerouslySkipPermissions => {
            let status = std::process::Command::new("claude")
                .arg("--dangerously-skip-permissions")
                .current_dir(&workspace_dir)
                .status()?;
            if !status.success() {
                return Err(Error::Git("claude exited with non-zero status".into()));
            }
        }
        ClaudeMode::None => {}
    }

    Ok(())
}

/// Copy configured files from each source repo into its corresponding worktree.
/// These are typically gitignored files (e.g. .env) that wouldn't be present in the worktree.
fn copy_base_files(
    copy_files: &[String],
    repos: &[crate::config::RepoRef],
    workspace_dir: &Path,
    quiet: bool,
) -> Result<()> {
    let mut copied_any = false;
    for repo in repos {
        let source = PathBuf::from(&repo.path);
        let target = workspace_dir.join(&repo.name);
        for file in copy_files {
            let src_path = source.join(file);
            if src_path.exists() {
                let dest_path = target.join(file);
                if let Some(parent) = dest_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(&src_path, &dest_path)?;
                copied_any = true;
            }
        }
    }
    if copied_any {
        info!(quiet, "  {} Copied extra files", "✓".green());
    }
    Ok(())
}

/// Merge .claude directories from each repo worktree into the workspace root.
///
/// - CLAUDE.md files are concatenated with repo name headers
/// - settings.local.json files have their allow/deny lists merged
/// - Subdirectories (e.g. skills/) are recursively copied and merged
/// - Other files are copied; conflicts are resolved by appending repo name
fn merge_claude_dirs(workspace_dir: &Path, worktrees: &[(PathBuf, PathBuf)], quiet: bool) -> Result<()> {
    let mut claude_md_parts: Vec<(String, String)> = Vec::new();
    let mut merged_allow: Vec<String> = Vec::new();
    let mut merged_deny: Vec<String> = Vec::new();
    let mut merged_mcp_servers: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    // Maps relative path (from .claude/) -> list of (repo_name, absolute_path)
    let mut other_files: HashMap<PathBuf, Vec<(String, PathBuf)>> = HashMap::new();
    let mut found_any = false;

    for (_source, target) in worktrees {
        let claude_dir = target.join(".claude");
        if !claude_dir.is_dir() {
            continue;
        }
        found_any = true;
        let repo_name = target
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        collect_claude_entries(
            &claude_dir,
            &claude_dir,
            &repo_name,
            &mut claude_md_parts,
            &mut merged_allow,
            &mut merged_deny,
            &mut merged_mcp_servers,
            &mut other_files,
        )?;
    }

    if !found_any {
        return Ok(());
    }

    let ws_claude_dir = workspace_dir.join(".claude");
    std::fs::create_dir_all(&ws_claude_dir)?;

    // Write merged CLAUDE.md
    if !claude_md_parts.is_empty() {
        let mut merged = String::new();
        for (repo_name, content) in &claude_md_parts {
            if !merged.is_empty() {
                merged.push_str("\n\n");
            }
            merged.push_str(&format!("# {} (from {})\n\n", "CLAUDE.md", repo_name));
            merged.push_str(content.trim());
        }
        merged.push('\n');
        std::fs::write(ws_claude_dir.join("CLAUDE.md"), &merged)?;
    }

    // Write merged settings.local.json
    if !merged_allow.is_empty() || !merged_deny.is_empty() {
        let mut settings = serde_json::Map::new();
        let mut permissions = serde_json::Map::new();
        if !merged_allow.is_empty() {
            merged_allow.sort();
            merged_allow.dedup();
            permissions.insert(
                "allow".to_string(),
                serde_json::Value::Array(merged_allow.into_iter().map(serde_json::Value::String).collect()),
            );
        }
        if !merged_deny.is_empty() {
            merged_deny.sort();
            merged_deny.dedup();
            permissions.insert(
                "deny".to_string(),
                serde_json::Value::Array(merged_deny.into_iter().map(serde_json::Value::String).collect()),
            );
        }
        settings.insert("permissions".to_string(), serde_json::Value::Object(permissions));
        let json = serde_json::to_string_pretty(&serde_json::Value::Object(settings))
            .map_err(|e| Error::Config(e.to_string()))?;
        std::fs::write(ws_claude_dir.join("settings.local.json"), format!("{}\n", json))?;
    }

    // Write merged mcp.json
    if !merged_mcp_servers.is_empty() {
        let mut mcp = serde_json::Map::new();
        mcp.insert(
            "mcpServers".to_string(),
            serde_json::Value::Object(merged_mcp_servers),
        );
        let json = serde_json::to_string_pretty(&serde_json::Value::Object(mcp))
            .map_err(|e| Error::Config(e.to_string()))?;
        std::fs::write(ws_claude_dir.join("mcp.json"), format!("{}\n", json))?;
    }

    // Copy other files (including those in subdirectories)
    for (rel_path, sources) in &other_files {
        let dest = ws_claude_dir.join(rel_path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if sources.len() == 1 {
            std::fs::copy(&sources[0].1, &dest)?;
        } else {
            // Multiple repos have the same relative path — prefix filename with repo name
            for (repo_name, path) in sources {
                let file_name = rel_path.file_name().unwrap().to_string_lossy();
                let prefixed = format!("{}.{}", repo_name, file_name);
                let dest = dest.with_file_name(prefixed);
                std::fs::copy(path, &dest)?;
            }
        }
    }

    info!(quiet, "  {} Merged .claude directories", "✓".green());

    Ok(())
}

/// Overlay a base-level .claude directory on top of the already-merged workspace .claude.
///
/// The base .claude dir lives at `~/.config/cutter/bases/<base_name>/.claude/`.
/// - CLAUDE.md: appended after repo-merged content
/// - settings.local.json: allow/deny entries merged into existing
/// - mcp.json: servers merged; base servers override same-named repo servers
/// - Other files: copied directly (overwrite on conflict)
fn overlay_base_claude_dir(workspace_dir: &Path, base_name: &str, quiet: bool) -> Result<()> {
    let base_claude_dir = crate::config::config_dir()?.join("bases").join(base_name).join(".claude");
    if !base_claude_dir.is_dir() {
        return Ok(());
    }

    let ws_claude_dir = workspace_dir.join(".claude");
    std::fs::create_dir_all(&ws_claude_dir)?;

    overlay_base_claude_dir_recursive(&base_claude_dir, &base_claude_dir, &ws_claude_dir)?;

    info!(quiet, "  {} Applied base .claude directory", "✓".green());
    Ok(())
}

/// Recursively walk the base .claude dir and overlay files onto the workspace .claude dir.
fn overlay_base_claude_dir_recursive(
    base_root: &Path,
    current_dir: &Path,
    ws_claude_dir: &Path,
) -> Result<()> {
    for entry in std::fs::read_dir(current_dir)? {
        let entry = entry?;
        let path = entry.path();
        let rel_path = path.strip_prefix(base_root).unwrap();

        if path.is_dir() {
            overlay_base_claude_dir_recursive(base_root, &path, ws_claude_dir)?;
        } else if path.is_file() {
            let rel_str = rel_path.to_string_lossy();
            let dest = ws_claude_dir.join(rel_path);

            if rel_str == "CLAUDE.md" {
                // Append base CLAUDE.md content after existing
                let base_content = std::fs::read_to_string(&path)?;
                let mut merged = String::new();
                if dest.exists() {
                    merged = std::fs::read_to_string(&dest)?;
                    if !merged.ends_with('\n') {
                        merged.push('\n');
                    }
                    merged.push('\n');
                }
                merged.push_str(&format!("# CLAUDE.md (from base)\n\n"));
                merged.push_str(base_content.trim());
                merged.push('\n');
                std::fs::write(&dest, &merged)?;
            } else if rel_str == "settings.local.json" {
                // Merge base settings into existing
                let mut allow = Vec::new();
                let mut deny = Vec::new();

                // Read existing workspace settings first
                if dest.exists() {
                    merge_settings_json(&dest, &mut allow, &mut deny)?;
                }
                // Merge base settings on top
                merge_settings_json(&path, &mut allow, &mut deny)?;

                // Write merged result
                allow.sort();
                allow.dedup();
                deny.sort();
                deny.dedup();

                let mut settings = serde_json::Map::new();
                let mut permissions = serde_json::Map::new();
                if !allow.is_empty() {
                    permissions.insert(
                        "allow".to_string(),
                        serde_json::Value::Array(allow.into_iter().map(serde_json::Value::String).collect()),
                    );
                }
                if !deny.is_empty() {
                    permissions.insert(
                        "deny".to_string(),
                        serde_json::Value::Array(deny.into_iter().map(serde_json::Value::String).collect()),
                    );
                }
                settings.insert("permissions".to_string(), serde_json::Value::Object(permissions));
                let json = serde_json::to_string_pretty(&serde_json::Value::Object(settings))
                    .map_err(|e| Error::Config(e.to_string()))?;
                std::fs::write(&dest, format!("{}\n", json))?;
            } else if rel_str == "mcp.json" {
                // Merge base MCP servers; base wins on conflict
                let mut servers = serde_json::Map::new();

                // Read existing workspace mcp.json first
                if dest.exists() {
                    let content = std::fs::read_to_string(&dest)?;
                    let value: serde_json::Value =
                        serde_json::from_str(&content).map_err(|e| Error::Config(e.to_string()))?;
                    if let Some(mcp_servers) = value.get("mcpServers").and_then(|v| v.as_object()) {
                        for (name, config) in mcp_servers {
                            servers.insert(name.clone(), config.clone());
                        }
                    }
                }

                // Read base mcp.json — base servers overwrite same-named entries
                let base_content = std::fs::read_to_string(&path)?;
                let base_value: serde_json::Value =
                    serde_json::from_str(&base_content).map_err(|e| Error::Config(e.to_string()))?;
                if let Some(mcp_servers) = base_value.get("mcpServers").and_then(|v| v.as_object()) {
                    for (name, config) in mcp_servers {
                        servers.insert(name.clone(), config.clone());
                    }
                }

                let mut mcp = serde_json::Map::new();
                mcp.insert("mcpServers".to_string(), serde_json::Value::Object(servers));
                let json = serde_json::to_string_pretty(&serde_json::Value::Object(mcp))
                    .map_err(|e| Error::Config(e.to_string()))?;
                std::fs::write(&dest, format!("{}\n", json))?;
            } else {
                // Other files: copy directly, overwriting if exists
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(&path, &dest)?;
            }
        }
    }
    Ok(())
}

/// Recursively collect entries from a .claude directory.
fn collect_claude_entries(
    base: &Path,
    dir: &Path,
    repo_name: &str,
    claude_md_parts: &mut Vec<(String, String)>,
    merged_allow: &mut Vec<String>,
    merged_deny: &mut Vec<String>,
    merged_mcp_servers: &mut serde_json::Map<String, serde_json::Value>,
    other_files: &mut HashMap<PathBuf, Vec<(String, PathBuf)>>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let rel_path = path.strip_prefix(base).unwrap().to_path_buf();

        if path.is_dir() {
            collect_claude_entries(base, &path, repo_name, claude_md_parts, merged_allow, merged_deny, merged_mcp_servers, other_files)?;
        } else if path.is_file() {
            let rel_str = rel_path.to_string_lossy();
            if rel_str == "CLAUDE.md" {
                let content = std::fs::read_to_string(&path)?;
                claude_md_parts.push((repo_name.to_string(), content));
            } else if rel_str == "settings.local.json" {
                merge_settings_json(&path, merged_allow, merged_deny)?;
            } else if rel_str == "mcp.json" {
                merge_mcp_json(&path, repo_name, merged_mcp_servers)?;
            } else {
                other_files
                    .entry(rel_path)
                    .or_default()
                    .push((repo_name.to_string(), path.clone()));
            }
        }
    }
    Ok(())
}

fn merge_mcp_json(
    path: &Path,
    repo_name: &str,
    servers: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let content = std::fs::read_to_string(path)?;
    let value: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| Error::Config(e.to_string()))?;

    if let Some(mcp_servers) = value.get("mcpServers").and_then(|v| v.as_object()) {
        for (name, config) in mcp_servers {
            let key = if servers.contains_key(name) {
                // Conflict: prefix with repo name to avoid overwriting
                format!("{}/{}", repo_name, name)
            } else {
                name.clone()
            };
            servers.insert(key, config.clone());
        }
    }

    Ok(())
}

fn merge_settings_json(path: &Path, allow: &mut Vec<String>, deny: &mut Vec<String>) -> Result<()> {
    let content = std::fs::read_to_string(path)?;
    let value: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| Error::Config(e.to_string()))?;

    if let Some(permissions) = value.get("permissions") {
        if let Some(arr) = permissions.get("allow").and_then(|v| v.as_array()) {
            for item in arr {
                if let Some(s) = item.as_str() {
                    allow.push(s.to_string());
                }
            }
        }
        if let Some(arr) = permissions.get("deny").and_then(|v| v.as_array()) {
            for item in arr {
                if let Some(s) = item.as_str() {
                    deny.push(s.to_string());
                }
            }
        }
    }

    Ok(())
}
