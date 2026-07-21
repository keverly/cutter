//! GitHub pull-request status for a workspace's repos, via the `gh` CLI.
//!
//! Each repo in a workspace is a worktree on the workspace's branch, so we query
//! `gh pr list --head <branch>` in each repo directory. Repos without a GitHub
//! remote (or without `gh` auth) simply yield nothing. Blocking — run off the UI
//! thread.

use std::path::PathBuf;
use std::process::Command;

use serde::Deserialize;

use crate::commands::claude;

/// The status we surface for a pull request. Closed-but-unmerged PRs aren't
/// represented — they're dropped (not "in flight", not requested for display).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Draft,
    Open,
    Merged,
}

impl PrState {
    pub fn label(self) -> &'static str {
        match self {
            PrState::Draft => "draft",
            PrState::Open => "open",
            PrState::Merged => "merged",
        }
    }
}

/// One pull request for a repo in a workspace.
#[derive(Debug, Clone)]
pub struct PrInfo {
    pub repo: String,
    pub number: u64,
    pub state: PrState,
    pub url: String,
    pub title: String,
}

/// Raw `gh pr list --json …` item.
#[derive(Deserialize)]
struct GhPr {
    number: u64,
    state: String,
    #[serde(rename = "isDraft")]
    is_draft: bool,
    url: String,
    #[serde(default)]
    title: String,
}

/// Map a gh item to a [`PrInfo`], dropping closed-but-unmerged PRs.
fn from_gh(repo: &str, gh: GhPr) -> Option<PrInfo> {
    let state = match gh.state.as_str() {
        "MERGED" => PrState::Merged,
        "OPEN" if gh.is_draft => PrState::Draft,
        "OPEN" => PrState::Open,
        _ => return None, // CLOSED (unmerged), etc.
    };
    Some(PrInfo {
        repo: repo.to_string(),
        number: gh.number,
        state,
        url: gh.url,
        title: gh.title,
    })
}

/// Parse a `gh pr list` JSON payload into infos for `repo`.
fn parse(repo: &str, json: &[u8]) -> Vec<PrInfo> {
    serde_json::from_slice::<Vec<GhPr>>(json)
        .map(|items| items.into_iter().filter_map(|it| from_gh(repo, it)).collect())
        .unwrap_or_default()
}

/// Fetch PR status for each `(repo_name, worktree_dir)` on `branch`. Runs `gh`
/// per repo; repos without a GitHub remote / auth just contribute nothing.
/// Blocking — call off the UI thread.
pub fn fetch(repos: &[(String, String)], branch: &str) -> Vec<PrInfo> {
    let gh = claude::find_binary("gh").unwrap_or_else(|| PathBuf::from("gh"));
    let path = claude::augmented_path(None);
    let mut out = Vec::new();
    for (name, dir) in repos {
        let output = Command::new(&gh)
            .env("PATH", &path)
            .current_dir(dir)
            .args([
                "pr", "list", "--head", branch, "--state", "all", "--limit", "30", "--json",
                "number,state,isDraft,url,title",
            ])
            .output();
        if let Ok(output) = output {
            if output.status.success() {
                out.extend(parse(name, &output.stdout));
            }
        }
    }
    out.sort_by_key(|p| p.number);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_filters_states() {
        let json = br#"[
            {"number":12,"state":"OPEN","isDraft":false,"url":"u12","title":"open one"},
            {"number":7,"state":"OPEN","isDraft":true,"url":"u7","title":"draft one"},
            {"number":3,"state":"MERGED","isDraft":false,"url":"u3","title":"merged one"},
            {"number":9,"state":"CLOSED","isDraft":false,"url":"u9","title":"closed one"}
        ]"#;
        let prs = parse("frontend", json);
        // Closed-unmerged dropped; three remain.
        assert_eq!(prs.len(), 3);
        let by_num: std::collections::HashMap<u64, PrState> =
            prs.iter().map(|p| (p.number, p.state)).collect();
        assert_eq!(by_num[&12], PrState::Open);
        assert_eq!(by_num[&7], PrState::Draft);
        assert_eq!(by_num[&3], PrState::Merged);
        assert!(!by_num.contains_key(&9));
        assert_eq!(prs[0].repo, "frontend");
    }

    #[test]
    fn bad_json_is_empty() {
        assert!(parse("r", b"not json").is_empty());
        assert!(parse("r", b"").is_empty());
    }
}
