use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Base '{0}' not found")]
    BaseNotFound(String),

    #[error("Base '{0}' already exists")]
    BaseAlreadyExists(String),

    #[error("Workspace '{0}' already exists")]
    WorkspaceAlreadyExists(String),

    #[error("Workspace '{0}' not found")]
    WorkspaceNotFound(String),

    #[error("Path is not a git repository: {0}")]
    NotAGitRepo(PathBuf),

    #[error("Path does not exist: {0}")]
    PathNotFound(PathBuf),

    #[error("Git error: {0}")]
    Git(String),

    #[error("Config error: {0}")]
    Config(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("TOML serialization error: {0}")]
    TomlSerialize(#[from] toml::ser::Error),

    #[error("TOML deserialization error: {0}")]
    TomlDeserialize(#[from] toml::de::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
