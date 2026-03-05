use colored::Colorize;
use std::path::PathBuf;
use tabled::{Table, Tabled};

use crate::error::Result;
use crate::git;
use crate::workspace::WorkspaceConfig;

#[derive(Tabled)]
struct StatusRow {
    #[tabled(rename = "Repo")]
    name: String,
    #[tabled(rename = "Branch")]
    branch: String,
    #[tabled(rename = "Status")]
    status: String,
}

pub fn run(name: &str) -> Result<()> {
    let ws = WorkspaceConfig::load(name)?;

    println!("Workspace: {} (base: {})\n", ws.workspace.name.bold(), ws.workspace.base);

    let mut rows = Vec::new();

    for repo in &ws.repos {
        let path = PathBuf::from(&repo.worktree_path);
        match git::status(&path) {
            Ok(s) => {
                let mut parts = Vec::new();
                if s.changed > 0 {
                    parts.push(format!("{} changed", s.changed));
                }
                if s.untracked > 0 {
                    parts.push(format!("{} untracked", s.untracked));
                }
                if s.ahead > 0 {
                    parts.push(format!("ahead {}", s.ahead));
                }
                if s.behind > 0 {
                    parts.push(format!("behind {}", s.behind));
                }
                let status = if parts.is_empty() {
                    "clean".green().to_string()
                } else {
                    parts.join(", ").yellow().to_string()
                };
                rows.push(StatusRow {
                    name: repo.name.clone(),
                    branch: s.branch,
                    status,
                });
            }
            Err(e) => {
                rows.push(StatusRow {
                    name: repo.name.clone(),
                    branch: "?".to_string(),
                    status: format!("error: {}", e).red().to_string(),
                });
            }
        }
    }

    println!("{}", Table::new(rows));
    Ok(())
}
