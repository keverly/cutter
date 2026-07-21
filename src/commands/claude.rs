//! Shared plumbing for driving a headless `claude` Code session — locating the
//! binary, building a PATH that resolves tools even from a Finder-launched GUI,
//! and running a one-shot capture. Both AI-driven workspace creation
//! ([`super::ai`]) and AI window linking ([`crate::ai_link`]) use these.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{Error, Result};

/// Locate the `claude` binary, honoring the `CUTTER_CLAUDE_BIN` override.
pub fn resolve_claude() -> PathBuf {
    if let Ok(p) = std::env::var("CUTTER_CLAUDE_BIN") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    find_binary("claude").unwrap_or_else(|| PathBuf::from("claude"))
}

/// Directories worth searching for CLI tools beyond `$PATH` — covers the case
/// where a Finder-launched GUI inherits only a minimal PATH.
pub fn common_bin_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".cargo/bin"));
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".claude/local"));
        dirs.push(home.join(".npm-global/bin"));
        dirs.push(home.join(".volta/bin"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    dirs.push(PathBuf::from("/usr/bin"));
    dirs
}

/// Find an executable by name on `$PATH` or in the common bin dirs.
pub fn find_binary(name: &str) -> Option<PathBuf> {
    let path_dirs = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    for dir in path_dirs.into_iter().chain(common_bin_dirs()) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Build a `PATH` with `extra_first` (and the common bin dirs) up front, so
/// `cutter`/`git`/`node`/`claude` resolve regardless of how this process was
/// launched.
pub fn augmented_path(extra_first: Option<&Path>) -> OsString {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = extra_first {
        dirs.push(d.to_path_buf());
    }
    dirs.extend(common_bin_dirs());
    if let Some(existing) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&existing));
    }
    // De-dup while preserving order.
    let mut seen = HashSet::new();
    dirs.retain(|d| seen.insert(d.clone()));
    std::env::join_paths(dirs).unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
}

/// Run `claude -p` headless with `prompt` on stdin and no tools, returning
/// captured stdout. Rides the user's Claude subscription (an env `ANTHROPIC_API_KEY`
/// would override it, so it's removed). Intended for small prompts whose output
/// we parse ourselves.
pub fn run_headless_capture(prompt: &str) -> Result<String> {
    let claude = resolve_claude();
    let path = augmented_path(None);
    let mut child = Command::new(&claude)
        .env_remove("ANTHROPIC_API_KEY")
        .env("PATH", &path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .arg("-p")
        .spawn()
        .map_err(|e| {
            Error::Git(format!(
                "could not launch `claude` ({e}). Is Claude Code installed and on your PATH?"
            ))
        })?;

    // The prompt is small (well under a pipe buffer), so writing it fully before
    // reading stdout won't deadlock. Dropping stdin closes the pipe.
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .map_err(|e| Error::Git(format!("failed to send prompt to claude: {e}")))?;
    }

    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        stdout
            .read_to_string(&mut out)
            .map_err(|e| Error::Git(format!("failed to read claude output: {e}")))?;
    }
    let status = child
        .wait()
        .map_err(|e| Error::Git(format!("claude session failed: {e}")))?;
    if !status.success() && out.trim().is_empty() {
        return Err(Error::Git(
            "claude session ended without output".into(),
        ));
    }
    Ok(out)
}
