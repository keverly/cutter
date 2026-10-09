pub mod cli;
pub mod commands;
pub mod config;
pub mod error;
pub mod git;
pub mod session;
pub mod workspace;

#[cfg(feature = "gui")]
pub mod ai_link;

#[cfg(feature = "gui")]
pub mod gui;

#[cfg(feature = "menubar")]
pub mod menubar;

#[cfg(any(feature = "gui", feature = "menubar"))]
pub mod pr;

#[cfg(feature = "gui")]
pub mod term_input;

#[cfg(feature = "gui")]
pub mod window_manager;
