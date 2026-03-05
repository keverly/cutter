use crate::error::{Error, Result};
use crate::workspace::WorkspaceConfig;

pub fn run(name: &str, claude: bool) -> Result<()> {
    let ws = WorkspaceConfig::load(name)?;

    if claude {
        let status = std::process::Command::new("claude")
            .current_dir(&ws.workspace.path)
            .status()?;
        if !status.success() {
            return Err(Error::Git("claude exited with non-zero status".into()));
        }
    } else {
        println!("{}", ws.workspace.path);
    }

    Ok(())
}
