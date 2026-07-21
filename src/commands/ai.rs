use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use colored::Colorize;

use crate::commands::claude;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::workspace::WorkspaceConfig;

/// Stand up a workspace from a natural-language request by driving a headless
/// Claude Code session.
///
/// Cutter wraps the user's `prompt` in fixed instructions (below), hands it to
/// `claude -p` with a *scoped* tool allowlist (read-only research tools plus
/// `cutter` and `git`), and lets Claude do any prep the request implies and then
/// run `cutter create <name> --base <base>` itself. We learn the name Claude
/// chose by diffing the set of workspaces before and after, and return it.
///
/// The session rides the user's Claude subscription — no API key needed. If
/// `ANTHROPIC_API_KEY` is set it would *override* the subscription, so we remove
/// it from the child's environment.
pub fn run(prompt: &str, base_hint: Option<&str>) -> Result<String> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err(Error::InvalidWorkspaceName("no prompt provided".into()));
    }

    let config = Config::load()?;
    if config.bases.is_empty() {
        return Err(Error::Git(
            "No bases configured. Add one with `cutter base add` first.".into(),
        ));
    }

    let cutter_bin = resolve_cutter()?;
    let claude_bin = claude::resolve_claude();
    let child_path = claude::augmented_path(cutter_bin.parent());

    // Names present before the session, so we can spot the one Claude creates.
    let before: HashSet<String> = list_workspace_names()?;

    let full_prompt = build_prompt(prompt, &config, base_hint);

    println!("🤖 Asking Claude to set up a workspace…\n");

    let mut child = Command::new(&claude_bin)
        // Ride the user's Claude subscription; an env API key would override it.
        .env_remove("ANTHROPIC_API_KEY")
        // Make `cutter` (and common tools) resolvable even when launched from a
        // Finder-spawned GUI with a minimal PATH.
        .env("PATH", &child_path)
        // Feed the prompt over stdin rather than a trailing positional arg:
        // `--allowedTools` is variadic and would otherwise swallow the prompt as
        // another tool name.
        .stdin(Stdio::piped())
        // Stream Claude's progress straight to the terminal.
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .args([
            "-p",
            "--allowedTools",
            "Bash(cutter:*),Bash(git:*),Read,Grep,Glob,WebFetch,WebSearch",
        ])
        .spawn()
        .map_err(|e| {
            Error::Git(format!(
                "could not launch `claude` ({e}). Is Claude Code installed and on your PATH?"
            ))
        })?;

    // Write the prompt and close the pipe so Claude starts immediately.
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(full_prompt.as_bytes())
            .map_err(|e| Error::Git(format!("failed to send prompt to claude: {e}")))?;
    }
    let status = child
        .wait()
        .map_err(|e| Error::Git(format!("claude session failed: {e}")))?;

    // Diff regardless of exit status — Claude may have created the workspace and
    // then hit a non-fatal snag afterward.
    let after: HashSet<String> = list_workspace_names()?;
    let mut created: Vec<String> = after.difference(&before).cloned().collect();
    created.sort();

    match created.len() {
        1 => {
            let name = created.remove(0);
            println!(
                "\n{} Workspace {} created via AI",
                "✓".green(),
                name.bold()
            );
            Ok(name)
        }
        0 => {
            if status.success() {
                Err(Error::Git(
                    "Claude finished but no new workspace was created.".into(),
                ))
            } else {
                Err(Error::Git(
                    "Claude session ended without creating a workspace.".into(),
                ))
            }
        }
        _ => {
            // Unusual, but don't hide it — report every new workspace.
            let joined = created.join(", ");
            println!(
                "\n{} Created multiple workspaces: {}",
                "✓".green(),
                joined.bold()
            );
            Ok(created.remove(0))
        }
    }
}

/// The hardcoded instructions wrapped around the user's request.
fn build_prompt(user_prompt: &str, config: &Config, base_hint: Option<&str>) -> String {
    let mut s = String::new();
    s.push_str(
        "You are helping set up a new \"cutter\" workspace on the user's machine. \
         Cutter is a git-worktree manager: `cutter create <name> --base <base>` \
         creates a workspace by adding a git worktree for every repo in <base>, all \
         on a new branch named <name>.\n\n\
         The user's request is at the very end of this message. Do the following:\n\n\
         1. Do any research the request implies using your read-only tools (read \
         files, fetch a URL, inspect repos with git log/status). Do NOT commit, \
         push, or otherwise modify any repository.\n\
         2. Choose a short, descriptive workspace name: kebab-case, ASCII lowercase \
         letters/digits/hyphens only, NO spaces, ideally 2-4 words. If the request \
         references a ticket id (e.g. a Linear ticket like ENG-1234), start the name \
         with the lowercased id, e.g. `eng-1234-sso-redirect`.\n\
         3. Pick exactly one base from the list below that best fits the request.\n\
         4. Create the workspace by running EXACTLY this command (bare `cutter`, no \
         path, no extra flags):\n\n\
         \x20      cutter create <name> --base <base>\n\n\
         \x20  If it fails because the name already exists, pick a slightly different \
         name and retry. Do not run any other `cutter` subcommand that changes state \
         (no `cutter remove`).\n\
         5. After it succeeds, state in one line the workspace name you created and \
         why you chose that base.\n\n",
    );

    s.push_str("Available bases:\n");
    for (name, base) in &config.bases {
        let repos: Vec<&str> = base.repos.iter().map(|r| r.name.as_str()).collect();
        s.push_str(&format!("- {} (repos: {})\n", name, repos.join(", ")));
    }

    if let Some(hint) = base_hint {
        s.push_str(&format!(
            "\nThe user pre-selected the base `{hint}`. Use it unless the request \
             clearly calls for a different one.\n"
        ));
    }

    s.push_str("\nUser request:\n");
    s.push_str(user_prompt);
    s
}

/// The names of every existing workspace.
fn list_workspace_names() -> Result<HashSet<String>> {
    Ok(WorkspaceConfig::list_all()?
        .into_iter()
        .map(|w| w.workspace.name)
        .collect())
}

/// Locate the `cutter` CLI binary so Claude can invoke it.
///
/// When we're running *as* the CLI, that's just us. From the GUI (`cutter-gui`)
/// it's a separate binary, so fall back to searching PATH and common install
/// locations.
fn resolve_cutter() -> Result<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if exe.file_name().and_then(|s| s.to_str()) == Some("cutter") {
            return Ok(exe);
        }
    }
    claude::find_binary("cutter").ok_or_else(|| {
        Error::Git(
            "Could not find the `cutter` CLI. Install it with `cargo install --path .` so the \
             AI session can create the workspace."
                .into(),
        )
    })
}
