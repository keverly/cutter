use crate::cli::ClaudeMode;
use crate::error::{Error, Result};
use crate::workspace::WorkspaceConfig;

pub fn run(name: &str, mode: ClaudeMode) -> Result<()> {
    let ws = WorkspaceConfig::load(name)?;

    match mode {
        ClaudeMode::None => {
            println!("{}", ws.workspace.path);
        }
        ClaudeMode::Normal => {
            let status = std::process::Command::new("claude")
                .current_dir(&ws.workspace.path)
                .status()?;
            if !status.success() {
                return Err(Error::Git("claude exited with non-zero status".into()));
            }
        }
        ClaudeMode::DangerouslySkipPermissions => {
            let status = std::process::Command::new("claude")
                .arg("--dangerously-skip-permissions")
                .current_dir(&ws.workspace.path)
                .status()?;
            if !status.success() {
                return Err(Error::Git("claude exited with non-zero status".into()));
            }
        }
    }

    Ok(())
}
