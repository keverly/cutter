use colored::Colorize;
use tabled::{Table, Tabled};

use crate::error::Result;
use crate::workspace::WorkspaceConfig;

#[derive(Tabled)]
struct WorkspaceRow {
    #[tabled(rename = "Workspace")]
    name: String,
    #[tabled(rename = "Base")]
    base: String,
    #[tabled(rename = "Branch")]
    branch: String,
    #[tabled(rename = "Repos")]
    repos: String,
    #[tabled(rename = "Path")]
    path: String,
}

pub fn run() -> Result<()> {
    let workspaces = WorkspaceConfig::list_all()?;

    if workspaces.is_empty() {
        println!("No workspaces. Use {} to create one.", "cutter create".bold());
        return Ok(());
    }

    let rows: Vec<WorkspaceRow> = workspaces
        .iter()
        .map(|ws| WorkspaceRow {
            name: ws.workspace.name.clone(),
            base: ws.workspace.base.clone(),
            branch: ws.workspace.branch.clone(),
            repos: ws.repos.len().to_string(),
            path: ws.workspace.path.clone(),
        })
        .collect();

    println!("{}", Table::new(rows));
    Ok(())
}
