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
                    if input.chars().any(|c| c.is_whitespace()) {
                        return Err("Name cannot contain whitespace".into());
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

    if name.chars().any(|c| c.is_whitespace()) {
        return Err(Error::InvalidWorkspaceName(
            "name cannot contain whitespace".into(),
        ));
    }

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

    // Assemble the workspace .claude: a generated CLAUDE.md that points at each
    // project's own CLAUDE.md, per-project namespaced skills/agents, and merged
    // settings/mcp.
    merge_claude_dirs(&workspace_dir, &created_worktrees, quiet)?;

    // Overlay base-level .claude directory on top of merged result
    overlay_base_claude_dir(&workspace_dir, &base_name, quiet)?;

    // Install Claude Code session-status hooks so Cutter can show whether a
    // session in this workspace is running or waiting for input. Runs after the
    // base overlay, which rewrites settings.local.json and would otherwise drop
    // the hooks. Best-effort: a failure here shouldn't fail workspace creation.
    match crate::session::ensure_hooks(&workspace_dir) {
        Ok(()) => info!(quiet, "  {} Installed session-status hooks", "✓".green()),
        Err(e) => info!(
            quiet,
            "  {} Could not install session-status hooks: {}",
            "!".yellow(),
            e
        ),
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
        linked_windows: Vec::new(),
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

/// Assemble the workspace's `.claude` directory from each repo's `.claude`.
///
/// - CLAUDE.md is *not* merged. Instead a workspace `.claude/CLAUDE.md` is
///   generated that points Claude at each project's own CLAUDE.md, keeping each
///   project's instructions authoritative and in place.
/// - `skills/` are flattened into `.claude/skills/`, each renamed with its repo
///   as a prefix (`<project>-<skill>`, matching the SKILL.md `name:`).
/// - `agents/` are namespaced per project: `.claude/agents/<project>/…`.
/// - settings.local.json (allow/deny) and mcp.json (servers) are merged.
/// - Any other files are copied by relative path; conflicts across repos are
///   resolved by prefixing the filename with the repo name.
fn merge_claude_dirs(workspace_dir: &Path, worktrees: &[(PathBuf, PathBuf)], quiet: bool) -> Result<()> {
    let mut merged_allow: Vec<String> = Vec::new();
    let mut merged_deny: Vec<String> = Vec::new();
    let mut merged_mcp_servers: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    // Maps relative path (from .claude/) -> list of (repo_name, absolute_path)
    let mut other_files: HashMap<PathBuf, Vec<(String, PathBuf)>> = HashMap::new();
    // (repo_name, relative path to its own CLAUDE.md if it has one), for the
    // generated pointer file.
    let mut projects: Vec<(String, Option<String>)> = Vec::new();

    let ws_claude_dir = workspace_dir.join(".claude");
    std::fs::create_dir_all(&ws_claude_dir)?;

    for (_source, target) in worktrees {
        let repo_name = target
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        // Locate this project's own CLAUDE.md (repo root preferred, then
        // .claude/) so the generated pointer can reference it.
        let claude_md_rel = if target.join("CLAUDE.md").is_file() {
            Some(format!("{repo_name}/CLAUDE.md"))
        } else if target.join(".claude").join("CLAUDE.md").is_file() {
            Some(format!("{repo_name}/.claude/CLAUDE.md"))
        } else {
            None
        };
        projects.push((repo_name.clone(), claude_md_rel));

        let claude_dir = target.join(".claude");
        if !claude_dir.is_dir() {
            continue;
        }

        // Skills land flat in the workspace's skills/, each directory renamed
        // (and its SKILL.md `name:` rewritten) with the repo as a prefix.
        let src_skills = claude_dir.join("skills");
        if src_skills.is_dir() {
            copy_repo_skills(&src_skills, &ws_claude_dir.join("skills"), &repo_name)?;
        }

        // Agents stay namespaced under a per-project subdirectory.
        let src_agents = claude_dir.join("agents");
        if src_agents.is_dir() {
            copy_dir_recursive(&src_agents, &ws_claude_dir.join("agents").join(&repo_name))?;
        }

        // Merge settings.local.json / mcp.json and collect any remaining files
        // (CLAUDE.md, skills/, and agents/ are handled above and skipped here).
        collect_claude_entries(
            &claude_dir,
            &claude_dir,
            &repo_name,
            &mut merged_allow,
            &mut merged_deny,
            &mut merged_mcp_servers,
            &mut other_files,
        )?;
    }

    // Generated CLAUDE.md pointing at each project's own instructions.
    let workspace_name = workspace_dir
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let claude_md = generate_workspace_claude_md(&workspace_name, &projects);
    std::fs::write(ws_claude_dir.join("CLAUDE.md"), claude_md)?;

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

    info!(quiet, "  {} Assembled .claude directory", "✓".green());

    Ok(())
}

/// Copy one repo's `.claude/skills/*` into the workspace's `.claude/skills/`,
/// flattened: a repo's `my-skill` becomes `<repo>-my-skill`, both as the
/// directory name and as the `name:` in its SKILL.md. Prefixing rather than
/// nesting keeps every skill discoverable at the top level while still saying
/// which project it came from, and keeps same-named skills from two repos apart.
///
/// Only directories are taken — a loose file sitting in `skills/` (a stray
/// `.DS_Store`, say) isn't a skill.
fn copy_repo_skills(src_skills: &Path, ws_skills: &Path, repo_name: &str) -> Result<()> {
    let prefix = skill_prefix(repo_name);
    for entry in std::fs::read_dir(src_skills)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let skill = entry.file_name().to_string_lossy().to_string();
        let dest = ws_skills.join(format!("{prefix}-{skill}"));
        copy_dir_recursive(&path, &dest)?;
        prefix_skill_md(&dest, &prefix)?;
    }
    Ok(())
}

/// A repo name reduced to a skill-name-safe prefix: lowercase, with any run of
/// non-alphanumerics collapsed to a single hyphen. Falls back to the raw name
/// if that leaves nothing.
fn skill_prefix(repo_name: &str) -> String {
    let mut out = String::with_capacity(repo_name.len());
    for c in repo_name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        repo_name.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Rewrite a copied skill's SKILL.md so its `name:` carries `prefix`. A skill
/// with no SKILL.md, an unreadable one, or one whose SKILL.md has no `name:`
/// frontmatter is left exactly as copied — plenty of skills are plain prose.
fn prefix_skill_md(skill_dir: &Path, prefix: &str) -> Result<()> {
    let path = skill_dir.join("SKILL.md");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    if let Some(updated) = prefix_skill_frontmatter_name(&content, prefix) {
        std::fs::write(&path, updated)?;
    }
    Ok(())
}

/// Prefix the `name:` value in a SKILL.md's YAML frontmatter, returning the new
/// file content — or `None` when there's nothing to rewrite: no frontmatter
/// block opening the file, no top-level `name:` inside it, or an empty value.
fn prefix_skill_frontmatter_name(content: &str, prefix: &str) -> Option<String> {
    let mut lines: Vec<&str> = content.split('\n').collect();
    // The frontmatter fence has to open the file.
    if lines.first()?.trim_end() != "---" {
        return None;
    }
    // `name:` counts only inside the block, so find the closing fence first.
    let close = 1 + lines.iter().skip(1).position(|l| l.trim_end() == "---")?;
    let idx = 1 + lines[1..close].iter().position(|l| l.starts_with("name:"))?;

    let raw = lines[idx]["name:".len()..].trim();
    // Keep whatever quoting style the file already used.
    let (quote, bare) = match raw.chars().next() {
        Some(q @ ('"' | '\'')) if raw.len() >= 2 && raw.ends_with(q) => {
            (Some(q), &raw[1..raw.len() - 1])
        }
        _ => (None, raw),
    };
    if bare.is_empty() {
        return None;
    }
    let renamed = match quote {
        Some(q) => format!("name: {q}{prefix}-{bare}{q}"),
        None => format!("name: {prefix}-{bare}"),
    };
    lines[idx] = &renamed;
    Some(lines.join("\n"))
}

/// Recursively copy a directory tree from `src` into `dest`, creating `dest`
/// and any parent directories. Existing files at `dest` are overwritten.
fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let target = dest.join(entry.file_name());
        if path.is_dir() {
            copy_dir_recursive(&path, &target)?;
        } else if path.is_file() {
            std::fs::copy(&path, &target)?;
        }
    }
    Ok(())
}

/// Build the generated workspace `CLAUDE.md`: a short pointer telling Claude to
/// read each project's own CLAUDE.md, plus a note on where each project's skills
/// and agents live.
fn generate_workspace_claude_md(
    workspace_name: &str,
    projects: &[(String, Option<String>)],
) -> String {
    let mut s = format!("# {workspace_name}\n\n");
    s.push_str(
        "This is a multi-project workspace created by Cutter. Each subdirectory \
         below is a separate git worktree for one project, and each project has \
         its own instructions. **Read each project's `CLAUDE.md` before working \
         in that project.**\n\n",
    );
    s.push_str("## Projects\n\n");
    for (name, claude_md) in projects {
        match claude_md {
            Some(rel) => s.push_str(&format!("- `{name}/` — read `{rel}`\n")),
            None => s.push_str(&format!("- `{name}/` — (no CLAUDE.md)\n")),
        }
    }
    s.push_str(
        "\n## Skills and agents\n\n\
         Skills contributed by each project sit at the top level of \
         `.claude/skills/`, prefixed with the project they came from — a \
         project's `my-skill` is `<project>-my-skill`, both as the directory \
         and as the skill's own `name:`.\n\n\
         Agents are namespaced by project: `.claude/agents/<project>/…`.\n",
    );
    s
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

/// Recursively collect settings.local.json / mcp.json / other files from a
/// .claude directory. CLAUDE.md and the top-level `skills/` and `agents/`
/// directories are skipped — they're handled separately by
/// [`merge_claude_dirs`] (generated pointer, prefixed skills, namespaced agents).
fn collect_claude_entries(
    base: &Path,
    dir: &Path,
    repo_name: &str,
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
            // skills/ and agents/ are namespaced per project elsewhere.
            let rel = rel_path.to_string_lossy();
            if rel == "skills" || rel == "agents" {
                continue;
            }
            collect_claude_entries(base, &path, repo_name, merged_allow, merged_deny, merged_mcp_servers, other_files)?;
        } else if path.is_file() {
            let rel_str = rel_path.to_string_lossy();
            if rel_str == "CLAUDE.md" {
                // Not merged: the generated workspace CLAUDE.md points at each
                // project's own CLAUDE.md instead.
                continue;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_frontmatter_name() {
        let md = "---\nname: verify\ndescription: Build and drive the app.\n---\n\n# Verifying\n";
        let out = prefix_skill_frontmatter_name(md, "mobile").unwrap();
        assert!(out.starts_with("---\nname: mobile-verify\ndescription:"));
        // Everything after the frontmatter is untouched.
        assert!(out.ends_with("---\n\n# Verifying\n"));
    }

    #[test]
    fn keeps_the_files_own_quoting() {
        let quoted = "---\nname: \"my-skill\"\n---\nbody\n";
        assert!(prefix_skill_frontmatter_name(quoted, "repo1")
            .unwrap()
            .contains("name: \"repo1-my-skill\""));
        let single = "---\nname: 'my-skill'\n---\nbody\n";
        assert!(prefix_skill_frontmatter_name(single, "repo1")
            .unwrap()
            .contains("name: 'repo1-my-skill'"));
    }

    #[test]
    fn leaves_skill_md_alone_when_theres_nothing_to_rewrite() {
        // No frontmatter at all — plenty of SKILL.md files are plain prose.
        assert!(prefix_skill_frontmatter_name("# Instructions\nDo the thing.\n", "backend").is_none());
        // Frontmatter without a name key.
        assert!(prefix_skill_frontmatter_name("---\ndescription: x\n---\nbody\n", "backend").is_none());
        // Empty name value.
        assert!(prefix_skill_frontmatter_name("---\nname:\n---\nbody\n", "backend").is_none());
        // Unterminated frontmatter.
        assert!(prefix_skill_frontmatter_name("---\nname: x\nbody\n", "backend").is_none());
    }

    #[test]
    fn only_the_frontmatter_name_is_touched() {
        // A `name:` in the body, after the block closes, must not be rewritten.
        let md = "---\nname: real\n---\n\nname: not-frontmatter\n";
        let out = prefix_skill_frontmatter_name(md, "portal").unwrap();
        assert!(out.contains("name: portal-real"));
        assert!(out.contains("\nname: not-frontmatter\n"));
    }

    #[test]
    fn copies_skills_flat_with_prefixed_dirs_and_names() {
        let tmp = std::env::temp_dir().join(format!("cutter-skills-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let src = tmp.join("repo/.claude/skills");
        let ws = tmp.join("ws/.claude/skills");

        // One skill with frontmatter, one plain-prose, plus nested files and a
        // stray non-skill file at the top of skills/.
        std::fs::create_dir_all(src.join("verify/references")).unwrap();
        std::fs::write(
            src.join("verify/SKILL.md"),
            "---\nname: verify\ndescription: d\n---\n\nbody\n",
        )
        .unwrap();
        std::fs::write(src.join("verify/references/notes.md"), "notes").unwrap();
        std::fs::create_dir_all(src.join("create-mr")).unwrap();
        std::fs::write(src.join("create-mr/SKILL.md"), "# Instructions\n").unwrap();
        std::fs::write(src.join(".DS_Store"), "junk").unwrap();

        copy_repo_skills(&src, &ws, "backend").unwrap();

        // Flat, prefixed directories — no per-repo folder in between.
        assert!(ws.join("backend-verify/SKILL.md").is_file());
        assert!(ws.join("backend-create-mr/SKILL.md").is_file());
        assert!(!ws.join("backend").exists());
        // Nested content comes along.
        assert_eq!(
            std::fs::read_to_string(ws.join("backend-verify/references/notes.md")).unwrap(),
            "notes"
        );
        // Frontmatter name rewritten; a prose SKILL.md left alone.
        let renamed = std::fs::read_to_string(ws.join("backend-verify/SKILL.md")).unwrap();
        assert!(renamed.starts_with("---\nname: backend-verify\n"));
        assert_eq!(
            std::fs::read_to_string(ws.join("backend-create-mr/SKILL.md")).unwrap(),
            "# Instructions\n"
        );
        // A loose file in skills/ isn't a skill.
        assert!(!ws.join("backend-.DS_Store").exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn repo_names_become_safe_prefixes() {
        assert_eq!(skill_prefix("backend"), "backend");
        assert_eq!(skill_prefix("branch-cut-admin"), "branch-cut-admin");
        assert_eq!(skill_prefix("My_Repo"), "my-repo");
        assert_eq!(skill_prefix("web.app v2"), "web-app-v2");
        // Nothing usable left: keep the raw name rather than emit a bare hyphen.
        assert_eq!(skill_prefix("!!!"), "!!!");
    }
}
