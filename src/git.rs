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

/// A repo's changes relative to the point its branch forked from, as raw
/// unified-diff text. See [`diff`].
#[derive(Clone)]
pub struct RepoDiff {
    /// The ref we diffed against, for display in the UI header (e.g. "main" or
    /// "origin/main"), or "HEAD" when we could only show uncommitted changes.
    pub base_label: String,
    /// Raw `git diff` output. Empty when there are no changes.
    pub text: String,
}

/// Diff a worktree against the commit its branch forked from, capturing every
/// change on the branch — committed *and* uncommitted — in one view.
///
/// `base_hint` is the ref the worktree was branched from (e.g. "main" or
/// "origin/main"), taken from the workspace's base config. We diff against the
/// merge base of that ref and HEAD (three-dot semantics), so commits that have
/// since landed on the base branch don't show up as spurious removals. When the
/// hint doesn't resolve (base renamed/deleted, brand-new repo), we fall back to
/// auto-detected default branches, and finally to plain `git diff HEAD`
/// (uncommitted changes only). Untracked files are not included.
pub fn diff(worktree: &Path, base_hint: Option<&str>) -> Result<RepoDiff> {
    let wt = worktree.to_string_lossy().to_string();

    // First base ref that actually resolves in this worktree wins.
    let base = base_hint
        .map(str::to_string)
        .into_iter()
        .chain(
            ["origin/HEAD", "origin/main", "origin/master", "main", "master"]
                .iter()
                .map(|s| s.to_string()),
        )
        .find(|r| rev_parse_commit(&wt, r).is_some());

    if let Some(base) = base {
        // Resolve the fork point ourselves (rather than `git diff --merge-base`,
        // which needs git 2.30+) and diff it against the working tree.
        if let Some(mb) = merge_base(&wt, &base) {
            let text = run_diff(&wt, &[&mb])?;
            return Ok(RepoDiff {
                base_label: base,
                text,
            });
        }
    }

    // No shared history to compare against: show only uncommitted work.
    let text = run_diff(&wt, &["HEAD"])?;
    Ok(RepoDiff {
        base_label: "HEAD".to_string(),
        text,
    })
}

/// The commit `r` resolves to in `wt`, or `None` if the ref doesn't exist.
fn rev_parse_commit(wt: &str, r: &str) -> Option<String> {
    let out = Command::new("git")
        .args([
            "-C",
            wt,
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{r}^{{commit}}"),
        ])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The best common ancestor of `base` and HEAD in `wt`.
fn merge_base(wt: &str, base: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["-C", wt, "merge-base", base, "HEAD"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Run `git diff <against...>` in `wt` and return its stdout.
fn run_diff(wt: &str, against: &[&str]) -> Result<String> {
    let mut args = vec!["-C", wt, "diff"];
    args.extend_from_slice(against);
    let output = Command::new("git").args(&args).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Git(format!("git diff failed: {}", stderr.trim())));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
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
