use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::config::workspaces_dir;
use crate::error::{Error, Result};

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    pub workspace: WorkspaceInfo,
    pub repos: Vec<WorkspaceRepo>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub name: String,
    pub base: String,
    pub branch: String,
    pub path: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceRepo {
    pub name: String,
    pub source: String,
    pub branch: String,
    pub worktree_path: String,
}

impl WorkspaceConfig {
    pub fn load(name: &str) -> Result<Self> {
        let path = workspace_config_path(name)?;
        if !path.exists() {
            return Err(Error::WorkspaceNotFound(name.to_string()));
        }
        let contents = std::fs::read_to_string(&path)?;
        Ok(toml::from_str(&contents)?)
    }

    pub fn save(&self) -> Result<()> {
        let dir = workspaces_dir()?;
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.toml", self.workspace.name));
        let contents = toml::to_string_pretty(self)?;
        std::fs::write(&path, contents)?;
        Ok(())
    }

    pub fn delete(name: &str) -> Result<()> {
        let path = workspace_config_path(name)?;
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }

    pub fn exists(name: &str) -> Result<bool> {
        Ok(workspace_config_path(name)?.exists())
    }

    pub fn list_all() -> Result<Vec<Self>> {
        let dir = workspaces_dir()?;
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut workspaces = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "toml") {
                let contents = std::fs::read_to_string(&path)?;
                if let Ok(ws) = toml::from_str(&contents) {
                    workspaces.push(ws);
                }
            }
        }
        workspaces.sort_by(|a: &Self, b: &Self| a.workspace.name.cmp(&b.workspace.name));
        Ok(workspaces)
    }
}

fn workspace_config_path(name: &str) -> Result<PathBuf> {
    Ok(workspaces_dir()?.join(format!("{name}.toml")))
}
