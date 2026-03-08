use std::path::Path;
use std::process::Command;

use crate::error::{Error, Result};

pub fn is_git_repo(path: &Path) -> bool {
    Command::new("git")
        .args(["-C", &path.to_string_lossy(), "rev-parse", "--git-dir"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub fn fetch(source: &Path) -> Result<()> {
    let output = Command::new("git")
        .args(["-C", &source.to_string_lossy(), "fetch"])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Git(format!(
            "Failed to fetch in '{}': {}",
            source.display(),
            stderr.trim()
        )));
    }
    Ok(())
}

pub fn worktree_add(source: &Path, target: &Path, branch: &str, start_point: Option<&str>) -> Result<()> {
    let mut args = vec![
        "-C".to_string(),
        source.to_string_lossy().to_string(),
        "worktree".to_string(),
        "add".to_string(),
        target.to_string_lossy().to_string(),
        "-b".to_string(),
        branch.to_string(),
    ];

    if let Some(sp) = start_point {
        args.push(sp.to_string());
    }

    let output = Command::new("git")
        .args(&args)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Git(format!(
            "Failed to add worktree for '{}': {}",
            source.display(),
            stderr.trim()
        )));
    }
    Ok(())
}

pub fn worktree_remove(source: &Path, target: &Path, force: bool) -> Result<()> {
    let target_str = target.to_string_lossy().to_string();
    let source_str = source.to_string_lossy().to_string();

    let mut cmd = Command::new("git");
    cmd.args(["-C", &source_str, "worktree", "remove"]);
    if force {
        cmd.arg("--force");
    }
    cmd.arg(&target_str);

    let output = cmd.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Git(format!(
            "Failed to remove worktree '{}': {}",
            target.display(),
            stderr.trim()
        )));
    }
    Ok(())
}

pub fn delete_branch(source: &Path, branch: &str) -> Result<()> {
    let output = Command::new("git")
        .args([
            "-C",
            &source.to_string_lossy(),
            "branch",
            "-d",
            branch,
        ])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Git(format!(
            "Failed to delete branch '{}': {}",
            branch,
            stderr.trim()
        )));
    }
    Ok(())
}

pub struct RepoStatus {
    pub branch: String,
    pub ahead: u32,
    pub behind: u32,
    pub changed: u32,
    pub untracked: u32,
}

pub fn status(path: &Path) -> Result<RepoStatus> {
    let output = Command::new("git")
        .args([
            "-C",
            &path.to_string_lossy(),
            "status",
            "--porcelain=v2",
            "--branch",
        ])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Git(format!("git status failed: {}", stderr.trim())));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut branch = String::from("(unknown)");
    let mut ahead = 0u32;
    let mut behind = 0u32;
    let mut changed = 0u32;
    let mut untracked = 0u32;

    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("# branch.head ") {
            branch = rest.to_string();
        } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() == 2 {
                ahead = parts[0].trim_start_matches('+').parse().unwrap_or(0);
                behind = parts[1].trim_start_matches('-').parse().unwrap_or(0);
            }
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            changed += 1;
        } else if line.starts_with("? ") {
            untracked += 1;
        }
    }

    Ok(RepoStatus {
        branch,
        ahead,
        behind,
        changed,
        untracked,
    })
}
