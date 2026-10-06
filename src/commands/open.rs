use std::path::Path;

use crate::cli::ClaudeMode;
use crate::commands::claude;
use crate::error::Result;
use crate::workspace::WorkspaceConfig;

pub fn run(name: &str, mode: ClaudeMode) -> Result<()> {
    let ws = WorkspaceConfig::load(name)?;

    match mode {
        ClaudeMode::None => println!("{}", ws.workspace.path),
        mode => claude::launch(Path::new(&ws.workspace.path), mode)?,
    }

    Ok(())
}
