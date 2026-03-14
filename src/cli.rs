use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ClaudeMode {
    None,
    Normal,
    DangerouslySkipPermissions,
}

#[derive(Parser)]
#[command(name = "cutter", about = "Git worktree workspace manager")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Manage base definitions
    Base {
        #[command(subcommand)]
        command: BaseCommand,
    },

    /// Create a new workspace from a base
    Create {
        /// Workspace name (also used as branch name)
        name: Option<String>,

        /// Base to create from
        #[arg(long)]
        base: Option<String>,

        /// Print workspace path after creation
        #[arg(long, group = "open_mode")]
        print: bool,

        /// Launch claude in workspace dir after creation
        #[arg(long, group = "open_mode")]
        open_claude: bool,

        /// Launch claude with --dangerously-skip-permissions in workspace dir after creation
        #[arg(long, group = "open_mode")]
        open_claude_dangerous: bool,
    },

    /// List all workspaces
    List,

    /// Show status of repos in a workspace
    Status {
        /// Workspace name (defaults to current workspace if omitted)
        name: Option<String>,
    },

    /// Remove a workspace
    Remove {
        /// Workspace name
        name: String,

        /// Keep workspace files on disk
        #[arg(long)]
        keep_files: bool,
    },

    /// Print workspace path
    Locate {
        /// Workspace name
        name: String,
    },

    /// Launch claude in a workspace directory
    OpenClaude {
        /// Workspace name
        name: String,

        /// Use --dangerously-skip-permissions
        #[arg(long)]
        dangerous: bool,
    },
}

#[derive(Subcommand)]
pub enum BaseCommand {
    /// Add a new base definition
    Add {
        /// Base name
        name: String,

        /// Paths to local git repositories
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },

    /// List all bases
    List,

    /// Remove a base definition
    Remove {
        /// Base name
        name: String,
    },
}
