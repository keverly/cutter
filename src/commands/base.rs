use colored::Colorize;
use tabled::{Table, Tabled};

use crate::config::{Base, Config, RepoRef, canonicalize_repo_path};
use crate::error::{Error, Result};
use crate::git;

#[derive(Tabled)]
struct BaseRow {
    #[tabled(rename = "Base")]
    name: String,
    #[tabled(rename = "Repos")]
    repos: String,
}

pub fn add(name: &str, paths: &[std::path::PathBuf]) -> Result<()> {
    let mut config = Config::load()?;

    if config.bases.contains_key(name) {
        return Err(Error::BaseAlreadyExists(name.to_string()));
    }

    let mut repos = Vec::new();
    for path in paths {
        let canonical = canonicalize_repo_path(path)?;

        if !git::is_git_repo(&canonical) {
            return Err(Error::NotAGitRepo(canonical));
        }

        let repo_name = canonical
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        repos.push(RepoRef {
            name: repo_name,
            path: canonical.to_string_lossy().to_string(),
        });
    }

    config.bases.insert(name.to_string(), Base { repos });
    config.save()?;

    println!("{} Base '{}' added", "✓".green(), name.bold());
    Ok(())
}

pub fn list() -> Result<()> {
    let config = Config::load()?;

    if config.bases.is_empty() {
        println!("No bases defined. Use {} to create one.", "cutter base add".bold());
        return Ok(());
    }

    let rows: Vec<BaseRow> = config
        .bases
        .iter()
        .map(|(name, base)| {
            let repos = base
                .repos
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            BaseRow {
                name: name.clone(),
                repos,
            }
        })
        .collect();

    println!("{}", Table::new(rows));
    Ok(())
}

pub fn remove(name: &str) -> Result<()> {
    let mut config = Config::load()?;

    if config.bases.remove(name).is_none() {
        return Err(Error::BaseNotFound(name.to_string()));
    }

    config.save()?;
    println!("{} Base '{}' removed", "✓".green(), name.bold());
    Ok(())
}
