//! AI-assisted linking of open windows to a workspace.
//!
//! Given the live windows and a workspace's context, pick the ones that belong:
//! any window whose open document lives inside the workspace directory (a
//! definite match), plus any window a headless Claude session judges related by
//! app/title (e.g. a browser tab about the same ticket). Existing links are
//! preserved. Claude failures degrade gracefully to directory matches only.

use std::path::Path;

use crate::commands::claude;
use crate::error::Result;
use crate::window_manager::WindowInfo;
use crate::workspace::LinkedWindow;

/// The workspace facts handed to the matcher.
pub struct WsContext {
    pub name: String,
    pub path: String,
    /// Repo directories belonging to the workspace (worktrees and their sources).
    pub repo_paths: Vec<String>,
}

/// Result of a suggestion pass.
pub struct Suggestion {
    /// The full link set to persist (existing links preserved, matches added).
    pub links: Vec<LinkedWindow>,
    /// Whether the Claude pass ran (false = degraded to directory matches only).
    pub claude_ok: bool,
}

/// Pick the windows to link to `ctx`. Directory matches are always included;
/// Claude adds app/title matches. `existing` links are preserved and de-duped.
pub fn suggest(ctx: &WsContext, windows: &[WindowInfo], existing: &[LinkedWindow]) -> Suggestion {
    let mut selected: Vec<bool> = windows.iter().map(|w| doc_in_workspace(w, ctx)).collect();

    let claude_ok = match ask_claude(ctx, windows) {
        Ok(picks) => {
            for i in picks {
                if let Some(slot) = selected.get_mut(i) {
                    *slot = true;
                }
            }
            true
        }
        Err(_) => false,
    };

    // Start from the existing links, append newly-picked ones not already present.
    let mut links = existing.to_vec();
    for (w, &sel) in windows.iter().zip(&selected) {
        if sel {
            let l = w.to_link();
            if !links.iter().any(|e| same_window(e, &l)) {
                links.push(l);
            }
        }
    }

    Suggestion { links, claude_ok }
}

/// Whether two link descriptors refer to the same window.
fn same_window(a: &LinkedWindow, b: &LinkedWindow) -> bool {
    if let (Some(x), Some(y)) = (a.window_id, b.window_id) {
        if x == y {
            return true;
        }
    }
    a.app_name == b.app_name && a.title == b.title && a.document_path == b.document_path
}

/// Whether a window's open document lives inside the workspace directory or one
/// of its repo directories.
fn doc_in_workspace(w: &WindowInfo, ctx: &WsContext) -> bool {
    let Some(doc) = &w.document_path else {
        return false;
    };
    let doc = Path::new(doc);
    doc.starts_with(&ctx.path) || ctx.repo_paths.iter().any(|p| doc.starts_with(p))
}

/// Ask Claude which window indices relate to the workspace.
fn ask_claude(ctx: &WsContext, windows: &[WindowInfo]) -> Result<Vec<usize>> {
    let prompt = build_prompt(ctx, windows);
    let out = claude::run_headless_capture(&prompt)?;
    Ok(parse_picks(&out, windows.len()))
}

fn build_prompt(ctx: &WsContext, windows: &[WindowInfo]) -> String {
    let mut s = String::new();
    s.push_str(
        "You are helping link open macOS windows to a software development workspace. \
         Decide which of the listed windows belong to this workspace.\n\n",
    );
    s.push_str(&format!("Workspace name: {}\n", ctx.name));
    s.push_str(&format!("Workspace directory: {}\n", ctx.path));
    if !ctx.repo_paths.is_empty() {
        s.push_str("Repository directories:\n");
        for p in &ctx.repo_paths {
            s.push_str(&format!("  - {p}\n"));
        }
    }
    s.push_str("\nOpen windows (index, app, title, and open file if any):\n");
    for (i, w) in windows.iter().enumerate() {
        let title = if w.title.trim().is_empty() {
            "(untitled)"
        } else {
            w.title.trim()
        };
        match &w.document_path {
            Some(doc) => s.push_str(&format!(
                "[{i}] app={:?} title={:?} file={:?}\n",
                w.app_name, title, doc
            )),
            None => s.push_str(&format!("[{i}] app={:?} title={:?}\n", w.app_name, title)),
        }
    }
    s.push_str(
        "\nGuidance:\n\
         - A window whose open file is inside the workspace or a repo directory definitely belongs.\n\
         - A title mentioning the workspace name, its ticket id (e.g. ENG-1234), or its clear topic \
           likely belongs (e.g. a browser tab or design doc about the same feature).\n\
         - Generic or unrelated windows (chat apps, music, mail, unrelated projects) do NOT belong \
           unless the title clearly references this work.\n\
         - When unsure, leave it out.\n\n\
         Respond with ONLY one line: the indices of the windows that belong, comma-separated, \
         prefixed with 'LINK:'. Example: 'LINK: 0, 3, 5'. If none belong, respond with 'LINK:' \
         and nothing after it.\n",
    );
    s
}

/// Parse a `LINK: 0, 3, 5` line out of Claude's output. Ignores out-of-range or
/// duplicate indices and anything that isn't the LINK line.
fn parse_picks(output: &str, n: usize) -> Vec<usize> {
    for line in output.lines() {
        let lower = line.to_ascii_lowercase();
        let Some(pos) = lower.find("link:") else {
            continue;
        };
        let rest = &line[pos + "link:".len()..];
        let mut picks = Vec::new();
        for tok in rest.split(|c: char| c == ',' || c.is_whitespace()) {
            if let Ok(i) = tok.trim().parse::<usize>() {
                if i < n && !picks.contains(&i) {
                    picks.push(i);
                }
            }
        }
        return picks;
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(app: &str, title: &str, doc: Option<&str>) -> WindowInfo {
        WindowInfo {
            pid: 1,
            app_name: app.to_string(),
            title: title.to_string(),
            document_path: doc.map(|s| s.to_string()),
            window_id: None,
        }
    }

    #[test]
    fn parses_link_line() {
        assert_eq!(parse_picks("LINK: 0, 2, 5", 6), vec![0, 2, 5]);
        // Prose around it, extra whitespace, out-of-range and dupes filtered.
        assert_eq!(
            parse_picks("Here you go:\nLINK: 1 1 3 9\n", 5),
            vec![1, 3]
        );
        // Case-insensitive, and empty means none.
        assert_eq!(parse_picks("link:", 5), Vec::<usize>::new());
        // No LINK line → nothing.
        assert_eq!(parse_picks("I think windows 0 and 1", 5), Vec::<usize>::new());
    }

    #[test]
    fn directory_match() {
        let ctx = WsContext {
            name: "feat".into(),
            path: "/Users/k/cutter/feat".into(),
            repo_paths: vec!["/Users/k/src/frontend".into()],
        };
        // Open file inside the workspace dir.
        assert!(doc_in_workspace(
            &win("Xcode", "App", Some("/Users/k/cutter/feat/frontend/App.xcodeproj")),
            &ctx
        ));
        // Open file inside a repo source dir.
        assert!(doc_in_workspace(
            &win("Xcode", "App", Some("/Users/k/src/frontend/App.xcodeproj")),
            &ctx
        ));
        // Unrelated file.
        assert!(!doc_in_workspace(
            &win("Xcode", "Other", Some("/Users/k/elsewhere/App.xcodeproj")),
            &ctx
        ));
        // No open document.
        assert!(!doc_in_workspace(&win("Chrome", "feat docs", None), &ctx));
        // Component-wise prefix: /cutter/feat must not match /cutter/feature.
        assert!(!doc_in_workspace(
            &win("Xcode", "App", Some("/Users/k/cutter/feature/App")),
            &ctx
        ));
    }

    #[test]
    fn suggest_includes_dir_matches_when_claude_absent() {
        // No real claude in the test env → ask_claude errors → claude_ok false,
        // but directory matches still come through.
        std::env::set_var("CUTTER_CLAUDE_BIN", "/nonexistent/claude-binary");
        let ctx = WsContext {
            name: "feat".into(),
            path: "/Users/k/cutter/feat".into(),
            repo_paths: vec![],
        };
        let windows = vec![
            win("Xcode", "App", Some("/Users/k/cutter/feat/App.xcodeproj")),
            win("Music", "Some Song", None),
        ];
        let s = suggest(&ctx, &windows, &[]);
        std::env::remove_var("CUTTER_CLAUDE_BIN");
        assert!(!s.claude_ok);
        assert_eq!(s.links.len(), 1);
        assert_eq!(s.links[0].app_name, "Xcode");
    }
}
