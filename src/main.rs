mod cli;
mod commands;
mod config;
mod error;
mod git;
mod workspace;

use clap::Parser;
use cli::{BaseCommand, Cli, Command};
use colored::Colorize;

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Base { command } => match command {
            BaseCommand::Add { name, paths } => commands::base::add(&name, &paths),
            BaseCommand::List => commands::base::list(),
            BaseCommand::Remove { name } => commands::base::remove(&name),
        },
        Command::Create {
            name,
            base,
            print,
            open_claude,
        } => commands::create::run(&name, &base, print, open_claude),
        Command::List => commands::list::run(),
        Command::Status { name } => {
            let name = match name {
                Some(n) => n,
                None => {
                    eprintln!("{} Please specify a workspace name", "Error:".red());
                    std::process::exit(1);
                }
            };
            commands::status::run(&name)
        }
        Command::Remove { name, keep_files } => commands::remove::run(&name, keep_files),
        Command::Locate { name } => commands::open::run(&name, false),
        Command::OpenClaude { name } => commands::open::run(&name, true),
    };

    if let Err(e) = result {
        eprintln!("{} {}", "Error:".red(), e);
        std::process::exit(1);
    }
}
