use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub settings: Settings,

    #[serde(default)]
    pub bases: BTreeMap<String, Base>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Settings {
    pub workspace_root: String,

    /// Default git ref to branch from when creating worktrees (e.g. "origin/main")
    #[serde(default = "default_branch_from")]
    pub default_branch_from: String,
}

fn default_branch_from() -> String {
    "origin/main".to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            workspace_root: "~/cutter".to_string(),
            default_branch_from: default_branch_from(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Base {
    pub repos: Vec<RepoRef>,

    /// Override the default git ref to branch from for this base
    #[serde(default)]
    pub branch_from: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoRef {
    pub name: String,
    pub path: String,
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_file_path()?;
        if path.exists() {
            let contents = std::fs::read_to_string(&path)?;
            Ok(toml::from_str(&contents)?)
        } else {
            Ok(Self {
                settings: Settings::default(),
                bases: BTreeMap::new(),
            })
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = config_file_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(self)?;
        std::fs::write(&path, contents)?;
        Ok(())
    }

    pub fn workspace_root(&self) -> PathBuf {
        expand_tilde(&self.settings.workspace_root)
    }
}

pub fn config_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("CUTTER_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    dirs::home_dir()
        .map(|d| d.join(".config").join("cutter"))
        .ok_or_else(|| Error::Config("Could not determine config directory".into()))
}

pub fn config_file_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

pub fn workspaces_dir() -> Result<PathBuf> {
    Ok(config_dir()?.join("workspaces"))
}

pub fn workspace_root_dir(config: &Config) -> PathBuf {
    if let Ok(root) = std::env::var("CUTTER_WORKSPACE_ROOT") {
        return PathBuf::from(root);
    }
    config.workspace_root()
}

pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

pub fn canonicalize_repo_path(path: &Path) -> Result<PathBuf> {
    path.canonicalize().map_err(|_| Error::PathNotFound(path.to_path_buf()))
}
